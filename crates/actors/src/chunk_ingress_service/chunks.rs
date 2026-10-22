use super::ChunkIngressMessage;
use super::ChunkIngressServiceInner;
use super::ingress_proofs::generate_and_store_ingress_proof_from_leaves;
use super::metrics::{
    record_chunk_duplicate, record_chunk_error, record_chunk_ingested, record_enqueue_duration,
    record_flush_failure, record_validation_duration,
};

use irys_database::{
    complete_ingress_leaves, confirm_data_size_for_data_root, db::IrysDatabaseExt as _,
};
use irys_domain::{StorageModule, WriteDataChunkError};
use irys_types::gossip::v2::GossipBroadcastMessageV2;
use irys_types::{
    ChunkPathHash, DataLedger, DataRoot, DatabaseProvider, H256, IngressMerkleLeaf, IngressProof,
    SendTraced as _, chunk::UnpackedChunk, expected_chunk_byte_range, hash_sha256,
    irys::IrysSigner, validate_path, validate_path_byte_range,
};
use irys_utils::ElapsedMs as _;
use lru::LruCache;
use rayon::prelude::*;
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
/// Two checks:
///   - `!data_size_confirmed`: the rightmost-chunk merkle validation hasn't
///     attested the size yet, so we don't know which chunks are required.
///   - `block_set.is_empty()`: cheap "has this data_root been seen in any
///     block yet?" gate.  `block_set` is a hint with two writers (mempool
///     `BlockConfirmed` pre-migration forward-fill + `persist_metadata`
///     canonical writer); a stale `BlockConfirmed` processed after a reorg
///     can leave an orphan hash here, so non-empty does NOT prove canonical
///     inclusion.  An over-permissive gate is correctness-safe: ingress
///     proofs are keyed on `data_root` (see the `IngressProofs` table), so a
///     proof generated for an orphan-tx data_root remains valid if any future
///     canonical tx references the same data_root — i.e. speculative, not
///     wasted.  Canonical truth is re-derived by
///     `tx_inclusion::find_canonical_ledger_range` at validation /
///     promotion time.  An empty set, by contrast, means no writer has ever
///     attached this data_root to a block, so we short-circuit.  After a
///     reorg that empties block_set, the next canonical confirmation
///     re-populates it via the mempool `BlockConfirmed` path
///     (`lifecycle.rs:96`).
fn should_skip_ingress_proof_generation(cdr: &irys_database::db_cache::CachedDataRoot) -> bool {
    !cdr.data_size_confirmed || cdr.block_set.is_empty()
}

/// Represents data_size information and if it comes from the publish ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataSizeInfo {
    /// The data_size value
    pub data_size: u64,
    /// Whether this data_size is from the publish ledger (trusted)
    pub is_from_publish_ledger: bool,
}

/// Pure post-park rescue decision for the TX/chunk gossip TOCTOU.
///
/// After a chunk is parked, we re-read `CachedDataRoot`. This function decides
/// whether to schedule another `ProcessPendingChunks` drain:
/// - `cdr_lookup = Err(_)` → leave parked (do **not** fail the already-accepted park)
/// - `cdr_lookup = Ok(None)` → leave parked (still waiting for the header)
/// - `cdr_lookup = Ok(Some(size))` → drain if `required_min_data_size` is `None`
///   (any CDR is enough) or `size >= required_min_data_size` (oversized-claim park)
///
/// Extracted so deleting the rescue path without updating tests fails CI.
#[must_use]
pub(crate) fn should_schedule_pending_drain_after_park(
    cdr_lookup: Result<Option<u64>, ()>,
    required_min_data_size: Option<u64>,
) -> bool {
    match cdr_lookup {
        Err(()) | Ok(None) => false,
        Ok(Some(data_size)) => match required_min_data_size {
            None => true,
            Some(min) => data_size >= min,
        },
    }
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
                | CriticalChunkIngressError::InvalidDataHash
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

async fn write_chunk_to_assigned_modules(
    modules: &[Arc<StorageModule>],
    chunk: &UnpackedChunk,
) -> Result<(), ChunkIngressError> {
    let mut wrote_any = false;
    let mut in_flight = false;
    for sm in modules {
        let writeable_offsets = match sm.get_writeable_offsets(chunk) {
            Ok(offsets) => offsets,
            Err(error) => {
                error!(
                    ?error,
                    storage_module = sm.id,
                    data_root = ?chunk.data_root,
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
            &chunk.data_root,
            &sm.id
        );
        match sm.write_data_chunk_queued(chunk).await {
            Ok(()) => wrote_any = true,
            Err(WriteDataChunkError::WritesPaused) => {}
            Err(WriteDataChunkError::Backpressure) => {
                return Err(AdvisoryChunkIngressError::Overloaded.into());
            }
            Err(error) => {
                error!(
                    "Failed to write chunk data_root {:?} tx_offset {} to storage_module {}: {:?}",
                    chunk.data_root, chunk.tx_offset, sm.id, error
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
    Ok(())
}

async fn remember_after_module_writes(
    recent_valid_chunks: &RwLock<LruCache<ChunkPathHash, ()>>,
    chunk_path_hash: ChunkPathHash,
    write_result: Result<(), ChunkIngressError>,
) -> Result<(), ChunkIngressError> {
    write_result?;
    recent_valid_chunks.write().await.put(chunk_path_hash, ());
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
    fn maybe_rescue_after_park(&self, data_root: DataRoot, required_min_data_size: Option<u64>) {
        let cdr_lookup = self
            .irys_db
            .view_eyre(|read_tx| irys_database::cached_data_root_by_data_root(read_tx, data_root))
            .map(|opt| opt.map(|cdr| cdr.data_size))
            .map_err(|e| {
                warn!(
                    chunk.data_root = %data_root,
                    error = ?e,
                    "post-park CDR recheck failed; chunk remains parked"
                );
            });

        if !should_schedule_pending_drain_after_park(cdr_lookup, required_min_data_size) {
            return;
        }

        debug!(
            chunk.data_root = %data_root,
            ?required_min_data_size,
            "CachedDataRoot usable after park; scheduling ProcessPendingChunks"
        );
        if let Err(e) = self
            .service_senders
            .chunk_ingress
            .send_traced(ChunkIngressMessage::ProcessPendingChunks(data_root))
        {
            warn!(
                chunk.data_root = %data_root,
                error = ?e,
                "Failed to schedule ProcessPendingChunks after post-park CDR recheck"
            );
        }
    }

    /// Limits on a chunk parked before its proof is checked.
    /// The missing-header park and the oversized-claim park both use these
    /// limits: `bytes` at most `chunk_size`, `data_path` at most the configured
    /// max, and `tx_offset` valid for the claimed size.
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
                &chunk.data_root,
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
                &chunk.data_root,
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
                &chunk.data_root, &chunk.tx_offset, *chunk.tx_offset, max_offset, chunk.data_size
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
        chunk: Arc<UnpackedChunk>,
        cap: usize,
        role: PendingDrainRole,
        kind: &'static str,
    ) -> Result<bool, IngressFailure> {
        let data_root = chunk.data_root;
        let tx_offset = chunk.tx_offset;
        let mut pending = self.pending_chunks.write().await;
        let current = pending.get(&data_root).map(lru::LruCache::len).unwrap_or(0);
        match pending.drain_admission(&data_root, current, cap, role == PendingDrainRole::Own) {
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
                pending.put(Arc::unwrap_or_clone(chunk));
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
        pending.put(Arc::unwrap_or_clone(chunk));
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
        data_root: DataRoot,
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
                .ingest_chunk(Arc::clone(&chunk), PendingDrainRole::Own)
                .await
            {
                Ok(()) => saw_ok = true,
                Err(failure) => {
                    record_chunk_error(failure.error.error_type(), failure.error.is_advisory());
                    error!(
                        "Failed to handle pending chunk ingress for data_root {:?}: {:?}",
                        data_root, failure.error
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
                if pending.get(&data_root).is_none() {
                    pending.put(chunk);
                    if cap_limited {
                        kept_cap_restore = true;
                    }
                    continue;
                }
                let parked = pending
                    .get_mut(&data_root)
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
                        chunk.data_root = %data_root,
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
            arrived_during_drain = pending.take_arrived_and_clear_drain(&data_root);
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
            chunk.data_root = %data_root,
            "Drained chunk put back; scheduling another ProcessPendingChunks"
        );
        schedule_pending_drain_retry(self.service_senders.clone(), data_root);
    }

    #[instrument(level = "info", skip_all, err(Debug), fields(chunk.data_root = ?chunk.data_root, chunk.tx_offset = ?chunk.tx_offset))]
    pub(crate) async fn handle_chunk_ingress_message(
        &self,
        chunk: UnpackedChunk,
    ) -> Result<(), ChunkIngressError> {
        self.ingest_chunk(Arc::new(chunk), PendingDrainRole::External)
            .await
            .map_err(|failure| failure.error)
    }

    async fn ingest_chunk(
        &self,
        chunk: Arc<UnpackedChunk>,
        role: PendingDrainRole,
    ) -> Result<(), IngressFailure> {
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

                // One span for the scan. Rayon workers inherit neither the span
                // nor the caller thread-local, so each worker enters both.
                let span = info_span!("index_read", caller = irys_domain::INGRESS_SIZE);
                let sm_data_sizes: Vec<DataSizeInfo> = span.in_scope(|| {
                    storage_modules_guard
                        .par_iter()
                        .filter_map(|sm| {
                            span.in_scope(|| {
                                irys_domain::with_index_caller(irys_domain::INGRESS_SIZE, || {
                                    let infos = sm.collect_data_root_infos(chunk.data_root).ok()?;
                                    if infos.0.is_empty() {
                                        return None;
                                    }

                                    let is_from_publish_ledger =
                                        sm.partition_assignment().is_some_and(|pa| {
                                            pa.ledger_id == Some(DataLedger::Publish.get_id())
                                        });

                                    // Find max data_size within this SM's infos
                                    let max_data_size =
                                        infos.0.iter().map(|info| info.data_size).max()?;

                                    Some(DataSizeInfo {
                                        data_size: max_data_size,
                                        is_from_publish_ledger,
                                    })
                                })
                            })
                        })
                        .collect()
                });

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
                    None => self.cached_data_size_after_scan(chunk.as_ref())?,
                }
            }
        };

        drop(_fetch_size_span);

        let data_size = match data_size {
            Some(ds) => ds,
            None => {
                // We don't have a data_root for this chunk but possibly the transaction containing this
                // chunks data_root will arrive soon. Park it in the pending chunks LRU cache until it does.
                self.check_unconfirmed_chunk_bounds(chunk.as_ref(), chunk_size)?;

                let preheader_chunks_per_item = std::cmp::min(
                    max_chunks_per_item,
                    self.config.mempool.max_preheader_chunks_per_item,
                );

                // The count check and the insert share one write lock so concurrent
                // ingress cannot exceed the per-item cap.
                let data_root = chunk.data_root;
                let tx_offset = chunk.tx_offset;
                let during_drain = self
                    .park_pending_chunk(
                        Arc::clone(&chunk),
                        preheader_chunks_per_item,
                        role,
                        "pre-header",
                    )
                    .await?;

                // A parked chunk is not stored. Log that, so Ok is not read as
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
                    self.maybe_rescue_after_park(data_root, None);
                }
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
                // Park the chunk for the time being — same TOCTOU as the
                // pre-header path if ProcessPendingChunks runs before this put.
                // Same cap as that path: `put` would LRU-evict an accepted sibling.
                // Same body, proof, and offset bounds: this branch returns
                // before those fields are checked.
                self.check_unconfirmed_chunk_bounds(chunk.as_ref(), chunk_size)?;
                let data_root = chunk.data_root;
                let claimed_size = chunk.data_size;
                let tx_offset = chunk.tx_offset;
                let preheader_chunks_per_item = std::cmp::min(
                    max_chunks_per_item,
                    self.config.mempool.max_preheader_chunks_per_item,
                );
                let during_drain = self
                    .park_pending_chunk(
                        Arc::clone(&chunk),
                        preheader_chunks_per_item,
                        role,
                        "oversized-claim",
                    )
                    .await?;
                info!(
                    chunk.data_root = %data_root,
                    chunk.tx_offset = %tx_offset,
                    claimed_data_size = claimed_size,
                    cached_data_size = data_size,
                    "Parked chunk: claims larger data_size than unconfirmed CachedDataRoot"
                );
                // Only re-drain if CDR grew enough to accept this claim;
                // otherwise leave parked for the larger TX's ProcessPendingChunks.
                // Skip this while a drain is active. That drain schedules one
                // follow-up; a second pop now would race its restore.
                if !during_drain {
                    self.maybe_rescue_after_park(data_root, Some(claimed_size));
                }
                return Ok(());
            }
        }

        // Validate the data_path/proof for the chunk, linking
        // data_root->chunk_hash
        let _proof_span = info_span!("chunk.validate_proof").entered();

        // Before confirmation, CachedDataRoot holds the largest size claimed by
        // any header sharing this root. Validate against this chunk's claim so
        // a rightmost data path can authoritatively correct that provisional
        // maximum downward. Once confirmed, only the cached size is accepted.
        let layout_data_size = if data_size_confirmed {
            data_size
        } else {
            chunk.data_size
        };
        if layout_data_size == 0 {
            error!(
                "Error: {:?}. Invalid data_size for data_root: {:?}. got 0 bytes",
                CriticalChunkIngressError::InvalidDataSize,
                chunk.data_root,
            );
            return Err(CriticalChunkIngressError::InvalidDataSize.into());
        }

        let (expected_min_byte_range, expected_max_byte_range) =
            expected_chunk_byte_range(chunk.tx_offset, layout_data_size, chunk_size)
                .map_err(|error| CriticalChunkIngressError::InvalidOffset(error.to_string()))?;
        let target_offset = u128::from(expected_max_byte_range - 1);
        let root_hash = chunk.data_root.0;
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

        let chunk_len = u64::try_from(chunk.bytes.len())
            .map_err(|_| CriticalChunkIngressError::InvalidChunkSize)?;

        // TODO: Mark the data_root as invalid if the chunk is an incorrect size
        // Someone may have created a data_root that seemed valid, but if the
        // data_path is valid but the chunk size doesn't mach the protocols
        // consensus size, then the data_root is actually invalid and no future
        // chunks from that data_root should be ingressed.

        let expected_chunk_len = expected_max_byte_range - expected_min_byte_range;
        if chunk_len != expected_chunk_len {
            error!(
                "Chunk has wrong size: tx_offset {} chunk_len {} expected {}",
                chunk.tx_offset, chunk_len, expected_chunk_len
            );
            return Ok(());
        }

        let is_last_chunk = expected_max_byte_range == layout_data_size;
        validate_path_byte_range(
            &path_result,
            (expected_min_byte_range, expected_max_byte_range),
        )
        .map_err(|error| CriticalChunkIngressError::InvalidOffset(error.to_string()))?;

        // Once the layout is confirmed, Merkle rightmost status must agree
        // with the transaction position. Before confirmation, a full prefix
        // chunk may validly belong to a larger root than an incorrect header
        // claims; accepting it preserves overlapping-data-root recovery while
        // the genuinely rightmost path remains the only size authority.
        if data_size_confirmed && path_result.is_rightmost_chunk != is_last_chunk {
            return Err(CriticalChunkIngressError::InvalidOffset(
                "data path rightmost status does not match the confirmed chunk position"
                    .to_string(),
            )
            .into());
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

        // A rightmost path attests the declared data size only after its exact
        // range and body have both been validated.
        if path_result.is_rightmost_chunk {
            self.irys_db
                .update_eyre(|db_tx| {
                    confirm_data_size_for_data_root(
                        db_tx,
                        &chunk.data_root,
                        expected_max_byte_range,
                    )
                })
                .map_err(|error| {
                    error!(
                        %error,
                        data_root = %chunk.data_root,
                        "Failed to confirm data-root size"
                    );
                    CriticalChunkIngressError::DatabaseError
                })?;
        }

        record_validation_duration(validation_start.elapsed_ms());

        drop(_proof_span);

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
                    chunk.data_root, chunk.tx_offset, e
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
                data_root = %chunk.data_root,
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
        remember_after_module_writes(&self.recent_valid_chunks, chunk_path_hash, write_result)
            .await
            .map_err(|error| IngressFailure {
                error,
                body_committed: true,
            })?;

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

        // Leaf persistence is assignment-independent: any staked node that
        // validates every chunk can produce the same proof it could before the
        // compact-leaf refactor. Storage-module durability separately authorizes
        // body reclamation; leaf presence alone is never an eviction fence.
        self.try_generate_ingress_proof_for_root(chunk_data_root, chunk_size)
            .map_err(|error| IngressFailure {
                error,
                body_committed: true,
            })?;

        // Storage-module writes retain their normal batching/striping policy.
        Ok(())
    }

    /// Checks whether an ingress proof should be generated for the given `data_root`
    /// and spawns proof generation if all prerequisites are met:
    /// - the CDR-only prerequisites in [`should_skip_ingress_proof_generation`]
    /// - no local proof already exists
    /// - every expected validated compact leaf is present with no gaps
    pub(crate) fn try_generate_ingress_proof_for_root(
        &self,
        data_root: H256,
        chunk_size: u64,
    ) -> Result<(), ChunkIngressError> {
        let cached_data_root = self
            .irys_db
            .view_eyre(|read_tx| irys_database::cached_data_root_by_data_root(read_tx, data_root))
            .map_err(|_| CriticalChunkIngressError::DatabaseError)?;

        // Early out: only generate ingress proofs for confirmed data sizes
        let Some(cdr) = cached_data_root else {
            return Ok(());
        };

        if should_skip_ingress_proof_generation(&cdr) {
            return Ok(());
        }

        // Be explicit that data_size used from here on is the confirmed data size
        let data_size = cdr.data_size;

        let local_address = self.config.irys_signer().address();
        let existing_local_proof = self
            .irys_db
            .view_eyre(|tx| {
                irys_database::ingress_proof_by_data_root_address(tx, data_root, local_address)
            })
            .map_err(|e| {
                error!(
                    "Database error checking ingress proof for data_root {:?}: {:?}",
                    data_root, e
                );
                CriticalChunkIngressError::DatabaseError
            })?;

        // Early return if we have a valid existing local proof
        if existing_local_proof.is_some() {
            self.ingress_proof_generation_state.clear_retries(data_root);
            info!(
                "Local ingress proof already exists and is valid for data root {}",
                &data_root
            );
            return Ok(());
        }

        // `complete_ingress_leaves` uses a dup-count only as a cheap prefilter,
        // then verifies every exact offset and byte boundary before returning.
        let leaves = self
            .irys_db
            .view_eyre(|tx| {
                complete_ingress_leaves(tx, data_root, local_address, data_size, chunk_size)
            })
            .map_err(|error| {
                error!(
                    ?data_root,
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
            let Some(generation_lease) = self.ingress_proof_generation_state.try_acquire(data_root)
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
                    data_root,
                    None,
                    leaves,
                    &gossip_sender,
                    generation_lease,
                ) {
                    Ok(_) => generation_state.clear_retries(data_root),
                    Err(error) if error.is_benign() => {
                        debug!(proof.data_root = ?data_root, "Skipped ingress proof generation: {error}");
                    }
                    Err(error) => {
                        warn!(proof.data_root = ?data_root, "Failed to generate ingress proof: {error}");
                        if let Some(delay) = generation_state.reserve_retry(data_root) {
                            let retry_state = generation_state.clone();
                            runtime_handle.spawn(async move {
                                tokio::time::sleep(delay).await;
                                retry_state.mark_retry_dispatched(data_root);
                                if let Err(send_error) = retry_sender.send_traced(
                                    ChunkIngressMessage::TryGenerateProofsForConfirmedRoots(vec![
                                        data_root,
                                    ]),
                                ) {
                                    retry_state.clear_retries(data_root);
                                    warn!(
                                        proof.data_root = ?data_root,
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

    /// The storage-module scan can outlive the header commit. Read the cached
    /// data root again before parking, or a chunk that sampled `None` stays on
    /// the park path after the header is visible.
    fn cached_data_size_after_scan(
        &self,
        chunk: &UnpackedChunk,
    ) -> Result<(Option<u64>, bool), CriticalChunkIngressError> {
        #[cfg(test)]
        if let Some(hook) = &self.after_missing_data_size_scan {
            hook();
        }
        let reread = self
            .irys_db
            .view_eyre(|read_tx| {
                irys_database::cached_data_root_by_data_root(read_tx, chunk.data_root)
            })
            .map_err(|error| {
                error!(
                    "Database error re-reading cached data_root for chunk data_root {:?} tx_offset {} after storage-module scan: {:?}",
                    chunk.data_root, chunk.tx_offset, error
                );
                CriticalChunkIngressError::DatabaseError
            })?;
        Ok(match reread {
            Some(cdr) => (Some(cdr.data_size), cdr.data_size_confirmed),
            None => (None, false),
        })
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
        // Same span on every worker: one trace for the scan, not one per module.
        let span = info_span!("index_read", caller = irys_domain::INGRESS_VERIFY);
        let confirmed_size = span.in_scope(|| {
            storage_modules.par_iter().find_map_any(|sm| {
                span.in_scope(|| {
                    irys_domain::with_index_caller(irys_domain::INGRESS_VERIFY, || {
                        let infos = sm
                            .collect_data_root_infos(data_root)
                            .ok()
                            .filter(|i| !i.0.is_empty())?;

                        for info in &infos.0 {
                            let relative_offset = (info.start_offset.0 as i64)
                                .saturating_add(last_chunk_index as i64);
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

                            if let Ok(result) =
                                validate_path(chunk_data_root.0, &data_path, target_byte_offset)
                                && result.is_rightmost_chunk
                            {
                                return Some(result.max_byte_range as u64);
                            }
                        }
                        None
                    })
                })
            })
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

/// Generates an ingress proof for a specific `data_root` from its ordered,
/// persisted compact leaves. Cached chunk bodies are not read on this path.
#[must_use = "the generated ingress proof should be used or stored"]
pub fn generate_ingress_proof(
    db: DatabaseProvider,
    data_root: DataRoot,
    leaves: Vec<IngressMerkleLeaf>,
    signer: IrysSigner,
    chain_id: ChainId,
    anchor: H256,
) -> eyre::Result<IngressProof> {
    let proof = irys_types::ingress::generate_ingress_proof_from_leaves(
        &signer, data_root, leaves, chain_id, anchor,
    )?;

    info!(
        "generated ingress proof {} for data root {}",
        &proof.proof, &data_root
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
    data_root: DataRoot,
) {
    tokio::spawn(async move {
        tokio::time::sleep(PENDING_DRAIN_RETRY_DELAY).await;
        if let Err(error) = senders
            .chunk_ingress
            .send_traced(ChunkIngressMessage::ProcessPendingChunks(data_root))
        {
            warn!(
                chunk.data_root = %data_root,
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
    use super::*;
    use irys_database::db_cache::CachedDataRoot;
    use irys_types::TxChunkOffset;
    use irys_types::UnixTimestamp;
    use proptest::prelude::*;
    use rstest::rstest;

    /// Build a `CachedDataRoot` with explicit gate-relevant fields.
    fn cdr_gate_fixture(
        data_size_confirmed: bool,
        block_set: Vec<H256>,
        txid_set: Vec<H256>,
        expiry_height: Option<u64>,
    ) -> CachedDataRoot {
        CachedDataRoot {
            data_size: 64,
            data_size_confirmed,
            txid_set,
            block_set,
            expiry_height,
            cached_at: UnixTimestamp::from_secs(0),
        }
    }

    /// Happy path: confirmed size + any block in `block_set` → proceed.
    #[test]
    fn ingress_gate_proceeds_when_size_confirmed_and_block_set_populated() {
        let cdr = cdr_gate_fixture(true, vec![H256::random()], vec![H256::random()], None);
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
            ChunkIngressError::Critical(CriticalChunkIngressError::InvalidDataHash),
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

    /// Pre-header park (no CDR at first look): any post-park CDR should drain.
    #[test]
    fn post_park_rescue_schedules_when_any_cdr_appears() {
        assert!(should_schedule_pending_drain_after_park(Ok(Some(12)), None));
        assert!(!should_schedule_pending_drain_after_park(Ok(None), None));
    }

    /// DB error after park must leave the chunk parked — not fail the ingest
    /// and not skip rescue via `?` (which would reintroduce the strand).
    #[test]
    fn post_park_rescue_leaves_parked_on_db_error() {
        assert!(!should_schedule_pending_drain_after_park(Err(()), None));
        assert!(!should_schedule_pending_drain_after_park(Err(()), Some(12)));
    }

    /// Oversized-claim park: only drain once CDR has grown to the claim.
    #[test]
    fn post_park_rescue_oversized_requires_sufficient_cdr_size() {
        assert!(!should_schedule_pending_drain_after_park(
            Ok(Some(10)),
            Some(12)
        ));
        assert!(should_schedule_pending_drain_after_park(
            Ok(Some(12)),
            Some(12)
        ));
        assert!(should_schedule_pending_drain_after_park(
            Ok(Some(20)),
            Some(12)
        ));
        assert!(!should_schedule_pending_drain_after_park(
            Ok(None),
            Some(12)
        ));
    }

    /// Confirmed-but-not-block-set state: pending tx with chunks staged but
    /// no `BlockConfirmed` has populated `block_set` yet.  Today the gate
    /// **short-circuits** — pinning this so a future widening to a
    /// `txid_set` + canonical-metadata fall-through is a deliberate change.
    #[test]
    fn ingress_gate_skips_when_txid_set_populated_but_block_set_empty() {
        let cdr = cdr_gate_fixture(true, vec![], vec![H256::random()], Some(100));
        assert!(
            should_skip_ingress_proof_generation(&cdr),
            "current contract: empty block_set short-circuits even when txid_set is populated"
        );
    }

    /// Unconfirmed data_size: skip regardless of `block_set` state.
    #[test]
    fn ingress_gate_skips_when_data_size_unconfirmed() {
        let cdr = cdr_gate_fixture(false, vec![H256::random()], vec![H256::random()], None);
        assert!(should_skip_ingress_proof_generation(&cdr));
    }

    /// Both gate inputs negative: skip.
    #[test]
    fn ingress_gate_skips_when_both_inputs_negative() {
        let cdr = cdr_gate_fixture(false, vec![], vec![H256::random()], None);
        assert!(should_skip_ingress_proof_generation(&cdr));
    }

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
                    ledger_id: Some(DataLedger::Submit.into()),
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
                data_root,
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
            data_root,
            data_size: 5,
            data_path: vec![4, 3, 2, 1].into(),
            bytes: vec![0, 1, 2, 3, 4].into(),
            tx_offset: TxChunkOffset::from(0),
        };
        let path_hash = chunk.chunk_path_hash();
        let recent = RwLock::new(LruCache::new(NonZeroUsize::new(8).expect("nonzero")));

        storage_module.fail_next_index_commit();
        let first = write_chunk_to_assigned_modules(&[Arc::clone(&storage_module)], &chunk).await;
        remember_after_module_writes(&recent, path_hash, first)
            .await
            .expect_err("SM Other must not remember the hash");
        assert!(
            !recent.read().await.contains(&path_hash),
            "retry must not hit recent_valid_chunks after a failed SM write"
        );

        remember_after_module_writes(
            &recent,
            path_hash,
            write_chunk_to_assigned_modules(&[storage_module], &chunk).await,
        )
        .await?;
        assert!(recent.read().await.contains(&path_hash));
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
        let data_root = chunk.data_root;
        let tx_offset = chunk.tx_offset;
        insert_unconfirmed_cdr(&inner.irys_db, data_root, chunk.data_size);
        inner.pending_chunks.write().await.put(chunk);

        inner.process_pending_chunks_for_root(data_root).await;

        let pending = inner.pending_chunks.read().await;
        let parked = pending
            .get(&data_root)
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
            ChunkIngressMessage::ProcessPendingChunks(root) if root == data_root
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
        let data_root = drained.data_root;
        insert_unconfirmed_cdr(&inner.irys_db, data_root, drained.data_size);
        {
            let mut pending = inner.pending_chunks.write().await;
            for offset in 1..=cap as u32 {
                pending.put(UnpackedChunk {
                    data_root,
                    data_size: drained.data_size,
                    data_path: Default::default(),
                    bytes: vec![offset as u8].into(),
                    tx_offset: TxChunkOffset::from(offset),
                });
            }
        }

        inner.finish_pending_drain(data_root, vec![drained]).await;

        let pending = inner.pending_chunks.read().await;
        let parked = pending
            .get(&data_root)
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
            ChunkIngressMessage::ProcessPendingChunks(root) if root == data_root
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
        let data_root = drained.data_root;
        insert_unconfirmed_cdr(&inner.irys_db, data_root, drained.data_size);
        {
            let mut pending = inner.pending_chunks.write().await;
            pending.put(UnpackedChunk {
                data_root,
                data_size: drained.data_size,
                data_path: Default::default(),
                bytes: vec![1_u8].into(),
                tx_offset: TxChunkOffset::from(0),
            });
            pending.put(UnpackedChunk {
                data_root,
                data_size: drained.data_size,
                data_path: Default::default(),
                bytes: vec![2_u8].into(),
                tx_offset: TxChunkOffset::from(1),
            });
        }

        inner.finish_pending_drain(data_root, vec![drained]).await;

        let pending = inner.pending_chunks.read().await;
        let parked = pending.get(&data_root).expect("root stays parked");
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
            ChunkIngressMessage::ProcessPendingChunks(root) if root == data_root
        ));
    }

    /// An oversized-claim chunk that arrives during a drain stays parked when
    /// the popped set still leaves room. One follow-up runs. The claim is still
    /// larger than the cached size, so the follow-up puts it back and stops.
    #[tokio::test]
    async fn oversized_claim_that_arrives_during_drain_stays_parked_and_one_follow_up_runs() {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-drain-claim")
            .build();
        let cap = 4_usize;
        let (inner, mut receivers) = test_ingress_inner(&tmp, cap, cap);
        let data_root = H256([0x44; 32]);
        insert_unconfirmed_cdr(&inner.irys_db, data_root, 32);
        park_offsets(&inner, data_root, [0_u32]).await;
        let (popped, ticket) = pop_for_drain(&inner, data_root).await;
        let incoming = chunk_with(data_root, 1, 0xAB, CLAIMED_DATA_SIZE);

        inner
            .handle_chunk_ingress_message(incoming)
            .await
            .expect("a claim under the cap must be kept while the root is draining");
        {
            let pending = inner.pending_chunks.read().await;
            let parked = pending
                .get(&data_root)
                .expect("the claim stays parked during the drain");
            assert!(parked.peek(&TxChunkOffset::from(1)).is_some());
            assert!(parked.peek(&TxChunkOffset::from(0)).is_none());
        }

        inner.finish_pending_drain(data_root, popped).await;
        drop(ticket);

        {
            let pending = inner.pending_chunks.read().await;
            let parked = pending.get(&data_root).expect("popped claim restored");
            assert_eq!(parked.len(), 2);
            assert_eq!(
                parked
                    .peek(&TxChunkOffset::from(0))
                    .expect("popped offset 0")
                    .bytes
                    .0,
                vec![0]
            );
            assert_eq!(
                parked
                    .peek(&TxChunkOffset::from(1))
                    .expect("arrived claim")
                    .bytes
                    .0,
                vec![0xAB]
            );
        }
        expect_delayed_redrain(&mut receivers, data_root).await;

        inner.process_pending_chunks_for_root(data_root).await;
        {
            let pending = inner.pending_chunks.read().await;
            let parked = pending
                .get(&data_root)
                .expect("the claim is still larger than the cached size");
            assert_eq!(parked.len(), 2);
        }
        expect_no_redrain(&mut receivers).await;
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
            .handle_chunk_ingress_message(incoming.clone())
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
            inner.pending_chunks.read().await.get(&data_root).is_none(),
            "a rejected chunk must not occupy the drained root"
        );

        inner.finish_pending_drain(data_root, popped).await;

        {
            let pending = inner.pending_chunks.read().await;
            let parked = pending.get(&data_root).expect("popped chunks restored");
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
            .handle_chunk_ingress_message(incoming)
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
        assert_eq!(pending.get(&data_root).expect("root parked").len(), cap);
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
            .handle_chunk_ingress_message(chunk_with(data_root, 0, 0xAB, CLAIMED_DATA_SIZE))
            .await
            .expect("the same offset parks while the root is draining");

        inner.finish_pending_drain(data_root, popped).await;
        drop(ticket);

        {
            let pending = inner.pending_chunks.read().await;
            let parked = pending.get(&data_root).expect("offset stays parked");
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
        inner.process_pending_chunks_for_root(data_root).await;
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
            .handle_chunk_ingress_message(chunk_with(data_root, 1, 1, CLAIMED_DATA_SIZE))
            .await
            .expect("the arrival parks before cancel");
        drop(popped);
        drop(ticket);

        {
            let pending = inner.pending_chunks.read().await;
            assert!(
                !pending.is_draining(&data_root),
                "cancel clears the drain flag"
            );
            let parked = pending
                .get(&data_root)
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

    /// The header can commit while the storage-module scan runs. The second
    /// read must see it and leave the park path.
    #[tokio::test]
    async fn preheader_chunk_uses_data_root_committed_during_the_storage_module_scan() {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-reread-cdr")
            .build();
        let (mut inner, _receivers) = test_ingress_inner(&tmp, 8, 8);
        let chunk = valid_single_chunk(vec![9_u8; 32]);
        let data_root = chunk.data_root;
        let data_size = chunk.data_size;
        let db = inner.irys_db.clone();
        inner.after_missing_data_size_scan = Some(std::sync::Arc::new(move || {
            insert_unconfirmed_cdr(&db, data_root, data_size);
        }));

        let err = inner
            .handle_chunk_ingress_message(chunk)
            .await
            .expect_err("a visible header takes the write path, and the test writer is closed");
        assert!(
            matches!(
                err,
                ChunkIngressError::Critical(CriticalChunkIngressError::DatabaseError)
            ),
            "{err:?}"
        );
        assert!(
            inner.pending_chunks.read().await.get(&data_root).is_none(),
            "the chunk must not be parked once the header is visible"
        );
    }

    /// The drain can start in the same window. A header that is visible after
    /// the scan is ingested, not rejected because the root is draining.
    #[tokio::test]
    async fn chunk_in_scan_uses_data_root_even_when_the_drain_starts() {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-reread-drain")
            .build();
        let (mut inner, _receivers) = test_ingress_inner(&tmp, 8, 8);
        let chunk = valid_single_chunk(vec![8_u8; 32]);
        let data_root = chunk.data_root;
        let data_size = chunk.data_size;
        let db = inner.irys_db.clone();
        let pending = std::sync::Arc::clone(&inner.pending_chunks);
        let ticket_slot: std::sync::Arc<
            std::sync::Mutex<Option<super::super::pending_chunks::DrainTicket>>,
        > = std::sync::Arc::new(std::sync::Mutex::new(None));
        let slot = std::sync::Arc::clone(&ticket_slot);
        inner.after_missing_data_size_scan = Some(std::sync::Arc::new(move || {
            insert_unconfirmed_cdr(&db, data_root, data_size);
            let guard = pending
                .try_write()
                .expect("pending lock is free during the scan");
            let ticket = guard.begin_drain(data_root, 1);
            *slot.lock().expect("ticket slot") = Some(ticket);
        }));

        let err = inner
            .handle_chunk_ingress_message(chunk)
            .await
            .expect_err("a visible header takes the write path, and the test writer is closed");
        assert!(
            matches!(
                err,
                ChunkIngressError::Critical(CriticalChunkIngressError::DatabaseError)
            ),
            "{err:?}"
        );
        assert!(
            inner.pending_chunks.read().await.get(&data_root).is_none(),
            "the chunk must not be parked once the header is visible"
        );
        drop(ticket_slot.lock().expect("ticket slot").take());
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
                .handle_chunk_ingress_message(chunk_with(
                    data_root,
                    offset,
                    offset as u8,
                    CLAIMED_DATA_SIZE,
                ))
                .await
                .expect("a chunk under the cap must be kept while the root is draining");
        }

        {
            let pending = inner.pending_chunks.read().await;
            let parked = pending
                .get(&data_root)
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

        inner.finish_pending_drain(data_root, popped).await;
        drop(ticket);

        {
            let pending = inner.pending_chunks.read().await;
            let parked = pending.get(&data_root).expect("popped chunk restored");
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

        inner.process_pending_chunks_for_root(data_root).await;
        {
            let pending = inner.pending_chunks.read().await;
            let parked = pending
                .get(&data_root)
                .expect("follow-up with no header leaves the chunks parked");
            assert_eq!(parked.len(), 3);
        }
        expect_no_redrain(&mut receivers).await;

        inner
            .handle_chunk_ingress_message(chunk_with(data_root, 3, 3, 128))
            .await
            .expect("after the drain, a chunk under the cap parks");
        {
            let pending = inner.pending_chunks.read().await;
            assert_eq!(
                pending.get(&data_root).expect("root parked").len(),
                4,
                "a park after the drain still fits under the cap"
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
        insert_unconfirmed_cdr(&inner.irys_db, data_root, 32);
        park_offsets(&inner, data_root, 0..=1).await;
        let popped = chunk_with(data_root, 0, 0xAB, CLAIMED_DATA_SIZE);

        inner.finish_pending_drain(data_root, vec![popped]).await;

        let pending = inner.pending_chunks.read().await;
        let parked = pending.get(&data_root).expect("root stays parked");
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
                pending.put(chunk_with(data_root, offset, offset as u8, data_size));
            }
        }

        for offset in cap as u32..=6 {
            inner
                .finish_pending_drain(
                    data_root,
                    vec![chunk_with(data_root, offset, offset as u8, data_size)],
                )
                .await;
        }

        let pending = inner.pending_chunks.read().await;
        let parked = pending.get(&data_root).expect("root stays parked");
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
                pending.put(chunk_with(data_root, offset, offset as u8, data_size));
            }
            let cache = pending.get_mut(&data_root).expect("root parked");
            cache.resize(std::num::NonZeroUsize::new(total).expect("over-cap size"));
            for offset in cap as u32..total as u32 {
                cache.put(
                    TxChunkOffset::from(offset),
                    chunk_with(data_root, offset, offset as u8, data_size),
                );
            }
        }

        inner.process_pending_chunks_for_root(data_root).await;

        {
            let pending = inner.pending_chunks.read().await;
            let parked = pending.get(&data_root).expect("popped set is restored");
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

        inner.process_pending_chunks_for_root(data_root).await;
        let pending = inner.pending_chunks.read().await;
        let parked = pending
            .get(&data_root)
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
                pending.put(chunk_with(
                    data_root,
                    offset,
                    offset as u8,
                    CLAIMED_DATA_SIZE,
                ));
            }
            let cache = pending.get_mut(&data_root).expect("root parked");
            cache.resize(std::num::NonZeroUsize::new(cap + 1).expect("room for the reject"));
            let mut oversized = chunk_with(data_root, 3, 0x03, CLAIMED_DATA_SIZE);
            oversized.bytes = vec![1_u8; 33].into();
            cache.put(TxChunkOffset::from(3), oversized);
        }

        inner.process_pending_chunks_for_root(data_root).await;

        let pending = inner.pending_chunks.read().await;
        let parked = pending.get(&data_root).expect("cap rejects are restored");
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

    /// One chunk is already in `recent_valid_chunks`, so its re-ingest returns
    /// Ok. The sibling is a cap restore. That schedules one drain. The next
    /// pass is cap-only and does not schedule again.
    #[tokio::test]
    async fn ok_chunk_and_cap_restore_schedules_one_drain_then_stops() {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-cap-one-drain")
            .build();
        let cap = 2_usize;
        let (inner, mut receivers) = test_ingress_inner(&tmp, cap, cap);
        let data_root = H256([0x46; 32]);
        // `chunk_path_hash` is the data_path hash. The two chunks need different
        // paths or both hit `recent_valid_chunks`.
        let mut seen = chunk_with(data_root, 0, 0, CLAIMED_DATA_SIZE);
        seen.data_path = vec![9_u8, 9, 9].into();
        let other = chunk_with(data_root, 1, 1, CLAIMED_DATA_SIZE);
        {
            let mut pending = inner.pending_chunks.write().await;
            pending.put(seen.clone());
            pending.put(other);
        }
        inner
            .recent_valid_chunks
            .write()
            .await
            .put(seen.chunk_path_hash(), ());

        inner.process_pending_chunks_for_root(data_root).await;

        {
            let pending = inner.pending_chunks.read().await;
            let parked = pending.get(&data_root).expect("cap sibling restored");
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

        inner.process_pending_chunks_for_root(data_root).await;
        let pending = inner.pending_chunks.read().await;
        let parked = pending
            .get(&data_root)
            .expect("second pass restores the sibling");
        assert_eq!(parked.len(), 1);
        assert!(parked.peek(&TxChunkOffset::from(1)).is_some());
        drop(pending);
        expect_no_redrain(&mut receivers).await;
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

        inner.process_pending_chunks_for_root(data_root).await;

        let pending = inner.pending_chunks.read().await;
        let parked = pending.get(&data_root).expect("full root parks again");
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

        inner.finish_pending_drain(data_root, vec![popped]).await;

        let pending = inner.pending_chunks.read().await;
        let parked = pending.get(&data_root).expect("root stays parked");
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
            .handle_chunk_ingress_message(incoming)
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
        let parked = pending.get(&data_root).expect("root stays parked");
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

    /// One chunk of the test config (`chunk_size` 32) is not enough for offsets 0..=2.
    const CLAIMED_DATA_SIZE: u64 = 96;

    fn chunk_with(data_root: H256, offset: u32, byte: u8, data_size: u64) -> UnpackedChunk {
        UnpackedChunk {
            data_root,
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
            pending.put(chunk_with(
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
        let chunks_map = pending.pop(&data_root).expect("parked root");
        let mut ticket = pending.begin_drain(data_root, chunks_map.len());
        let senders = inner.service_senders.clone();
        ticket.set_gained_follow_up(move || {
            schedule_pending_drain_retry(senders, data_root);
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
            ChunkIngressMessage::ProcessPendingChunks(root) if root == data_root
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
            data_root: H256([0x11; 32]),
            data_size: 33,
            data_path: Default::default(),
            bytes: vec![1_u8; 33].into(),
            tx_offset: TxChunkOffset::from(0),
        };
        let data_root = chunk.data_root;
        let tx_offset = chunk.tx_offset;
        inner.pending_chunks.write().await.put(chunk);

        inner.process_pending_chunks_for_root(data_root).await;

        let pending = inner.pending_chunks.read().await;
        assert!(
            pending
                .get(&data_root)
                .and_then(|chunks| chunks.peek(&tx_offset))
                .is_none(),
            "validation failure must stay dropped"
        );
        assert!(receivers.chunk_ingress.try_recv().is_err());
    }

    /// A larger unconfirmed claim must not LRU-evict an already parked sibling.
    #[tokio::test]
    async fn larger_claim_park_rejects_when_preheader_cap_is_full() {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-claim-cap")
            .build();
        let (inner, _receivers) = test_ingress_inner(&tmp, 2, 2);
        let data_root = H256([0x22; 32]);
        insert_unconfirmed_cdr(&inner.irys_db, data_root, 10);
        {
            let mut pending = inner.pending_chunks.write().await;
            for offset in 0..2_u32 {
                pending.put(UnpackedChunk {
                    data_root,
                    data_size: 20,
                    data_path: Default::default(),
                    bytes: vec![offset as u8].into(),
                    tx_offset: TxChunkOffset::from(offset),
                });
            }
        }
        // `CLAIMED_DATA_SIZE` covers offset 2 at the test `chunk_size` of 32.
        // A smaller claim is an invalid offset and never reaches the cap.
        let incoming = UnpackedChunk {
            data_root,
            data_size: CLAIMED_DATA_SIZE,
            data_path: Default::default(),
            bytes: vec![9_u8].into(),
            tx_offset: TxChunkOffset::from(2),
        };
        let err = inner
            .handle_chunk_ingress_message(incoming)
            .await
            .expect_err("full root must reject the larger-claim park");
        assert!(
            matches!(
                err,
                ChunkIngressError::Advisory(AdvisoryChunkIngressError::PreHeaderOffsetExceedsCap)
            ),
            "{err:?}"
        );
        let pending = inner.pending_chunks.read().await;
        let parked = pending.get(&data_root).expect("root stays parked");
        assert!(parked.peek(&TxChunkOffset::from(0)).is_some());
        assert!(parked.peek(&TxChunkOffset::from(1)).is_some());
        assert!(parked.peek(&TxChunkOffset::from(2)).is_none());
        assert_eq!(parked.len(), 2);
    }

    /// Under the cap, the larger-claim path still parks the chunk.
    #[tokio::test]
    async fn larger_claim_park_stores_when_under_cap() {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-claim-under-cap")
            .build();
        let (inner, _receivers) = test_ingress_inner(&tmp, 2, 2);
        let data_root = H256([0x33; 32]);
        insert_unconfirmed_cdr(&inner.irys_db, data_root, 10);
        inner.pending_chunks.write().await.put(UnpackedChunk {
            data_root,
            data_size: 20,
            data_path: Default::default(),
            bytes: vec![1_u8].into(),
            tx_offset: TxChunkOffset::from(0),
        });
        // `CLAIMED_DATA_SIZE` covers offset 1 at the test `chunk_size` of 32.
        let incoming = UnpackedChunk {
            data_root,
            data_size: CLAIMED_DATA_SIZE,
            data_path: Default::default(),
            bytes: vec![2_u8].into(),
            tx_offset: TxChunkOffset::from(1),
        };
        inner
            .handle_chunk_ingress_message(incoming)
            .await
            .expect("under-cap larger claim parks");
        let pending = inner.pending_chunks.read().await;
        let parked = pending.get(&data_root).expect("root stays parked");
        assert!(parked.peek(&TxChunkOffset::from(0)).is_some());
        assert!(parked.peek(&TxChunkOffset::from(1)).is_some());
        assert_eq!(parked.len(), 2);
    }

    /// Unconfirmed cached size, claim one byte larger. The body is not
    /// proof-checked on this path, so a body over `chunk_size` is rejected
    /// and not parked.
    #[tokio::test]
    async fn oversized_claim_rejects_body_over_chunk_size() {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-claim-oversize-bytes")
            .build();
        let (inner, mut receivers) = test_ingress_inner(&tmp, 8, 8);
        let data_root = H256([0xC2; 32]);
        insert_unconfirmed_cdr(&inner.irys_db, data_root, 32);
        let mut incoming = chunk_with(data_root, 0, 0xAB, 33);
        // Test config `chunk_size` is 32. One byte over must not park.
        incoming.bytes = vec![1_u8; 33].into();

        let err = inner
            .handle_chunk_ingress_message(incoming)
            .await
            .expect_err("a claim whose body exceeds chunk_size must be rejected");
        assert!(
            matches!(
                err,
                ChunkIngressError::Advisory(AdvisoryChunkIngressError::PreHeaderOversizedBytes)
            ),
            "{err:?}"
        );
        assert!(
            inner.pending_chunks.read().await.get(&data_root).is_none(),
            "a rejected claim must not be parked"
        );
        assert!(receivers.chunk_ingress.try_recv().is_err());
    }

    /// Same unconfirmed claim, with a proof longer than the pre-header max.
    #[tokio::test]
    async fn oversized_claim_rejects_data_path_over_preheader_max() {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-claim-oversize-path")
            .build();
        let (inner, mut receivers) = test_ingress_inner(&tmp, 8, 8);
        let data_root = H256([0xC3; 32]);
        insert_unconfirmed_cdr(&inner.irys_db, data_root, 32);
        let mut incoming = chunk_with(data_root, 0, 0xAB, 64);
        let max_path = inner.config.mempool.max_preheader_data_path_bytes;
        incoming.data_path = vec![0_u8; max_path + 1].into();

        let err = inner
            .handle_chunk_ingress_message(incoming)
            .await
            .expect_err("a claim whose data_path exceeds the pre-header max must be rejected");
        assert!(
            matches!(
                err,
                ChunkIngressError::Advisory(AdvisoryChunkIngressError::PreHeaderOversizedDataPath)
            ),
            "{err:?}"
        );
        assert!(
            inner.pending_chunks.read().await.get(&data_root).is_none(),
            "a rejected claim must not be parked"
        );
        assert!(receivers.chunk_ingress.try_recv().is_err());
    }

    /// `chunk_size` 32 and `data_size` 32 admit only offset 0.
    #[tokio::test]
    async fn oversized_claim_rejects_offset_past_claimed_size() {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-claim-bad-offset")
            .build();
        let (inner, mut receivers) = test_ingress_inner(&tmp, 8, 8);
        let data_root = H256([0xC4; 32]);
        insert_unconfirmed_cdr(&inner.irys_db, data_root, 16);
        let incoming = chunk_with(data_root, 1, 0xAB, 32);

        let err = inner
            .handle_chunk_ingress_message(incoming)
            .await
            .expect_err("a claim whose tx_offset is past the claimed size must be rejected");
        assert!(
            matches!(
                err,
                ChunkIngressError::Advisory(AdvisoryChunkIngressError::PreHeaderInvalidOffset(_))
            ),
            "{err:?}"
        );
        assert!(
            inner.pending_chunks.read().await.get(&data_root).is_none(),
            "a rejected claim must not be parked"
        );
        assert!(receivers.chunk_ingress.try_recv().is_err());
    }

    /// A claim inside the body, proof, and offset bounds is still parked.
    /// Bytes equal `chunk_size` and the proof equals the configured max.
    #[tokio::test]
    async fn oversized_claim_parks_when_body_path_and_offset_fit() {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("ingress-claim-bounded")
            .build();
        let (inner, mut receivers) = test_ingress_inner(&tmp, 8, 8);
        let data_root = H256([0xC5; 32]);
        insert_unconfirmed_cdr(&inner.irys_db, data_root, 32);
        // 64 bytes is two chunks of 32, so offset 1 is the last valid offset.
        let mut incoming = chunk_with(data_root, 1, 0xAB, 64);
        incoming.bytes = vec![1_u8; 32].into();
        let max_path = inner.config.mempool.max_preheader_data_path_bytes;
        incoming.data_path = vec![0_u8; max_path].into();

        inner
            .handle_chunk_ingress_message(incoming)
            .await
            .expect("a bounded oversized claim must still park");
        let pending = inner.pending_chunks.read().await;
        let parked = pending.get(&data_root).expect("claim is parked");
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
            "cached size is still below the claim, so no drain is scheduled"
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
            pending_chunks: std::sync::Arc::new(RwLock::new(
                super::super::PriorityPendingChunks::new(8, preheader_cap),
            )),
            pending_ingress_proofs: std::sync::Mutex::new(
                super::super::pending_ingress_proofs::PendingIngressProofs::new(8),
            ),
            chunk_data_writer: super::super::chunk_data_writer::ChunkDataWriter::closed_for_test(),
            ingress_proof_generation_state: super::super::IngressProofGenerationState::new(),
            http_ingress_open: std::sync::Arc::new(std::sync::Mutex::new(true)),
            after_missing_data_size_scan: None,
        };
        (inner, receivers)
    }

    fn insert_unconfirmed_cdr(db: &DatabaseProvider, data_root: H256, data_size: u64) {
        use irys_database::cache_data_root;
        use irys_database::db::IrysDatabaseExt as _;
        use irys_types::{
            DataTransactionHeader, DataTransactionHeaderV1, DataTransactionHeaderV1WithMetadata,
            DataTransactionMetadata,
        };

        let header = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                data_root,
                data_size,
                ..Default::default()
            },
            metadata: DataTransactionMetadata::new(),
        });
        db.update_eyre(|tx| cache_data_root(tx, &header, None).map(|_| ()))
            .expect("insert cached data root");
    }

    fn valid_single_chunk(bytes: Vec<u8>) -> UnpackedChunk {
        let chunk_size = bytes.len();
        let leaves = irys_types::generate_leaves(std::iter::once(Ok(bytes.clone())), chunk_size)
            .expect("leaves");
        let root = irys_types::generate_data_root(leaves).expect("root");
        let data_root = H256(root.id);
        let proof = irys_types::resolve_proofs(root, None)
            .expect("proofs")
            .into_iter()
            .next()
            .expect("one proof");
        UnpackedChunk {
            data_root,
            data_size: chunk_size as u64,
            data_path: proof.proof.into(),
            bytes: bytes.into(),
            tx_offset: TxChunkOffset::from(0),
        }
    }
}
