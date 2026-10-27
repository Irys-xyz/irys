use irys_database::db::IrysDatabaseExt as _;
use irys_database::db_cache::data_size_to_chunk_count;
use irys_database::submodule::tables::DataRootInfo;
use irys_database::{
    cached_chunk_by_chunk_offset, cached_data_root_by_commitment, cached_data_roots_by_tree_root,
};
use irys_types::data_tree::data_commitment;
use irys_types::{
    ChunkFormat, Config, DataCommitment, DataLedger, DataTreeRoot, LedgerChunkOffset,
    LedgerChunkRange, PackedChunk, PartitionChunkOffset, RelativeChunkOffset, TxChunkOffset,
    UnpackedChunk, app_state::DatabaseProvider,
};
use nodit::interval::ii;
use opentelemetry::{KeyValue, global, metrics::Counter};
use std::collections::BTreeMap;
use std::sync::{
    LazyLock,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};
use tracing::{debug, warn};

use crate::models::index_read_metrics::{self, SERVE_DATA_ROOT, SERVE_LEDGER, SERVE_SPAN};
use crate::{
    ChunkMissingOnDisk, StorageModulesReadGuard, checked_add_i32_u64,
    get_overlapped_storage_modules, get_storage_module_at_offset,
};

static CHUNK_READS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    global::meter("irys-domain")
        .u64_counter("irys.chunk_read_total")
        .with_description(
            "Chunk body reads by caller (serve_ledger, serve_span), source (storage_module, cache), and outcome (hit, miss, error)",
        )
        .build()
});

fn note_chunk_read(caller: &'static str, source: &'static str, outcome: &'static str) {
    CHUNK_READS.add(
        1,
        &[
            KeyValue::new("caller", caller),
            KeyValue::new("source", source),
            KeyValue::new("outcome", outcome),
        ],
    );
    #[cfg(test)]
    CHUNK_READ_NOTES.with(|notes| notes.borrow_mut().push((caller, source, outcome)));
}

/// One exhausted cache reader fails every hole in the span. Count each offset,
/// but log the first one and then at most once a second.
fn warn_omitted_cache_read(
    ledger_offset: LedgerChunkOffset,
    partition_offset: u32,
    error: &eyre::Report,
) {
    use std::sync::OnceLock;

    const INTERVAL: Duration = Duration::from_secs(1);
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    static LAST_MS: AtomicU64 = AtomicU64::new(0);

    let elapsed_ms = u64::try_from(ORIGIN.get_or_init(Instant::now).elapsed().as_millis())
        .unwrap_or(u64::MAX)
        .max(1);
    let previous = LAST_MS.load(Ordering::Relaxed);
    if previous != 0 && elapsed_ms.saturating_sub(previous) < INTERVAL.as_millis() as u64 {
        return;
    }
    LAST_MS.store(elapsed_ms, Ordering::Relaxed);
    warn!(
        error = %error,
        ledger.offset = *ledger_offset,
        partition.offset = partition_offset,
        "cache read failed; omitting offset from chunk span"
    );
}

#[cfg(test)]
thread_local! {
    static CACHE_READ_FAULT: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
    static CHUNK_READ_NOTES: std::cell::RefCell<Vec<(&'static str, &'static str, &'static str)>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// Remaining `collect_data_root_infos` reads that fail before the database.
    static DATA_ROOT_INDEX_FAULTS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn set_cache_read_fault(tx_offset: Option<u32>) {
    CACHE_READ_FAULT.with(|cell| cell.set(tx_offset));
}

#[cfg(test)]
fn set_data_root_index_faults(count: u32) {
    DATA_ROOT_INDEX_FAULTS.with(|cell| cell.set(count));
}

/// `true` once. The next placement read fails and the count drops by one.
#[cfg(test)]
fn take_data_root_index_fault() -> bool {
    DATA_ROOT_INDEX_FAULTS.with(|cell| {
        let left = cell.get();
        if left == 0 {
            return false;
        }
        cell.set(left - 1);
        true
    })
}

#[cfg(test)]
fn take_chunk_read_notes() -> Vec<(&'static str, &'static str, &'static str)> {
    CHUNK_READ_NOTES.with(|notes| std::mem::take(&mut *notes.borrow_mut()))
}

/// `true` when `error` is a storage hole: the index names a body this file does not hold.
pub fn missing_on_disk(error: &eyre::Report) -> bool {
    error
        .chain()
        .any(|source| source.downcast_ref::<ChunkMissingOnDisk>().is_some())
}

/// How a lookup names the data it wants.
#[derive(Clone, Copy, Debug)]
enum PlacementLookup {
    /// One data commitment: a point lookup.
    Commitment(DataCommitment),
    /// Every commitment of one data tree root: a scan of the data-root rows.
    TreeRoot(DataTreeRoot),
}

/// Placements of `lookup` in one storage module, with their data commitments.
fn read_data_root_placements(
    sm: &crate::StorageModule,
    lookup: PlacementLookup,
) -> eyre::Result<Vec<(DataCommitment, DataRootInfo)>> {
    #[cfg(test)]
    if take_data_root_index_fault() {
        return Err(eyre::eyre!("injected data root index error"));
    }
    match lookup {
        PlacementLookup::Commitment(data_commitment) => Ok(sm
            .collect_data_root_infos(data_commitment)?
            .0
            .into_iter()
            .map(|info| (data_commitment, info))
            .collect()),
        PlacementLookup::TreeRoot(data_root) => sm.collect_data_root_infos_for_tree_root(data_root),
    }
}

/// A root-only chunk lookup found chunks of more than one data size at the
/// offset. The caller must name the size.
#[derive(Debug, thiserror::Error)]
#[error(
    "data root {data_tree_root} has chunks of more than one data size at offset {tx_offset} \
     (sizes {data_sizes:?}); pass dataSize to choose one"
)]
pub struct AmbiguousDataSize {
    pub data_tree_root: DataTreeRoot,
    pub tx_offset: TxChunkOffset,
    pub data_sizes: Vec<u64>,
}

fn unpack_packed(config: &Config, packed: &PackedChunk) -> eyre::Result<UnpackedChunk> {
    let consensus = &config.consensus;
    irys_packing::unpack(
        packed,
        consensus.entropy_packing_iterations,
        consensus.chunk_size as usize,
        consensus.chain_id,
    )
}

/// Provides chunks to `actix::web` front end (mostly)
#[derive(Debug, Clone)]
pub struct ChunkProvider {
    /// Collection of storage modules for distributing chunk data
    pub storage_modules_guard: StorageModulesReadGuard,
    /// Main node DB — used for cache-backed data_root fetches when no local SM
    /// has the body (upload/API / proof-generator nodes often lack a ledger
    /// assignment but still hold `CachedChunks` long enough to serve peers).
    pub db: DatabaseProvider,
    pub config: Config,
}

impl ChunkProvider {
    pub fn new(
        config: Config,
        storage_modules_guard: StorageModulesReadGuard,
        db: DatabaseProvider,
    ) -> Self {
        Self {
            config,
            storage_modules_guard,
            db,
        }
    }

    /// Packed chunk for one ledger offset.
    ///
    /// `Ok(None)` when no storage module covers the offset, or the module has
    /// no body on disk. This route does not read the chunk cache and does not
    /// compute packing entropy. `Err` is a real read failure, not a hole.
    pub fn get_chunk_by_ledger_offset(
        &self,
        ledger: DataLedger,
        ledger_offset: LedgerChunkOffset,
    ) -> eyre::Result<Option<PackedChunk>> {
        index_read_metrics::with_caller(SERVE_LEDGER, || {
            self.ledger_in_caller(ledger, ledger_offset)
        })
    }

    fn ledger_in_caller(
        &self,
        ledger: DataLedger,
        ledger_offset: LedgerChunkOffset,
    ) -> eyre::Result<Option<PackedChunk>> {
        let Some(module) =
            get_storage_module_at_offset(&self.storage_modules_guard, ledger, ledger_offset)
        else {
            note_chunk_read(SERVE_LEDGER, "storage_module", "miss");
            return Ok(None);
        };
        match module.generate_full_chunk_ledger_offset(ledger_offset) {
            Ok(Some(packed)) => {
                note_chunk_read(SERVE_LEDGER, "storage_module", "hit");
                Ok(Some(packed))
            }
            Ok(None) => {
                note_chunk_read(SERVE_LEDGER, "storage_module", "miss");
                Ok(None)
            }
            Err(error) if missing_on_disk(&error) => {
                debug!(
                    error = %error,
                    ledger.offset = *ledger_offset,
                    "ledger chunk indexed without a body"
                );
                note_chunk_read(SERVE_LEDGER, "storage_module", "miss");
                Ok(None)
            }
            Err(error) => {
                note_chunk_read(SERVE_LEDGER, "storage_module", "error");
                Err(error)
            }
        }
    }

    /// Unpacked chunks for the inclusive ledger span `[from, to]`, ordered by
    /// ledger offset. A storage read error is omitted when another offset is
    /// present or absent, and returned when every offset failed to read.
    pub fn get_unpacked_chunks_by_ledger_span(
        &self,
        ledger: DataLedger,
        from: LedgerChunkOffset,
        to: LedgerChunkOffset,
    ) -> eyre::Result<Vec<(LedgerChunkOffset, UnpackedChunk)>> {
        index_read_metrics::with_caller(SERVE_SPAN, || self.span_in_caller(ledger, from, to))
    }

    fn span_in_caller(
        &self,
        ledger: DataLedger,
        from: LedgerChunkOffset,
        to: LedgerChunkOffset,
    ) -> eyre::Result<Vec<(LedgerChunkOffset, UnpackedChunk)>> {
        let requested_len = (*to).saturating_sub(*from).saturating_add(1);
        let requested = LedgerChunkRange(ii(from, to));
        let modules =
            get_overlapped_storage_modules(&self.storage_modules_guard, ledger, &requested);
        let mut out = Vec::new();
        let mut read_failures = 0_u64;
        let mut last_read_error: Option<eyre::Report> = None;
        for module in modules {
            let module_range = match module.get_storage_module_ledger_offsets() {
                Ok(range) => range,
                Err(error) => {
                    debug!(
                        error = %error,
                        "storage module has no ledger range; skipping it"
                    );
                    note_chunk_read(SERVE_SPAN, "storage_module", "error");
                    continue;
                }
            };
            let Some(overlap) = nodit::InclusiveInterval::intersection(&module_range, &requested)
            else {
                continue;
            };
            let partition_range = match module.make_range_partition_relative(overlap) {
                Ok(range) => range,
                Err(error) => {
                    debug!(
                        error = %error,
                        "storage module range is not partition-relative; skipping it"
                    );
                    note_chunk_read(SERVE_SPAN, "storage_module", "error");
                    continue;
                }
            };
            let start = partition_range.start();
            let end = partition_range.end();
            // A one-offset overlap errors when the index names a chunk the disk
            // does not hold. Read that module offset by offset and keep the rest.
            let mut batch = match module.generate_full_chunks(start, end) {
                Ok(packed) => Some(packed),
                Err(error) => {
                    debug!(
                        error = %error,
                        "chunk span read failed; reading each offset"
                    );
                    None
                }
            };
            let ledger_start = *module_range.start();
            let mut cursor = start.0;
            loop {
                let partition_offset = PartitionChunkOffset(cursor);
                let ledger_offset = LedgerChunkOffset::from(ledger_start + u64::from(cursor));
                // `Could not find chunk bytes on disk` is a hole: the index
                // names a body this file does not hold. Any other error is a
                // failed read.
                let mut read_failed: Option<eyre::Report> = None;
                let packed = if let Some(map) = batch.as_mut() {
                    map.remove(&partition_offset)
                } else {
                    match module.generate_full_chunk(partition_offset) {
                        Ok(chunk) => chunk,
                        Err(error) if missing_on_disk(&error) => {
                            debug!(
                                error = %error,
                                partition.offset = cursor,
                                "chunk offset has no body on disk"
                            );
                            note_chunk_read(SERVE_SPAN, "storage_module", "miss");
                            None
                        }
                        Err(error) => {
                            debug!(
                                error = %error,
                                partition.offset = cursor,
                                "chunk offset read failed"
                            );
                            note_chunk_read(SERVE_SPAN, "storage_module", "error");
                            read_failed = Some(error);
                            None
                        }
                    }
                };
                if let Some(packed) = packed {
                    match unpack_packed(&self.config, &packed) {
                        Ok(unpacked) => {
                            note_chunk_read(SERVE_SPAN, "storage_module", "hit");
                            out.push((ledger_offset, unpacked));
                        }
                        Err(error) => {
                            note_chunk_read(SERVE_SPAN, "storage_module", "error");
                            read_failures += 1;
                            last_read_error = Some(error);
                        }
                    }
                } else if let Some(error) = read_failed {
                    match self.cached_unpacked_at(SERVE_SPAN, &module, partition_offset) {
                        Ok(Some(cached)) => out.push((ledger_offset, cached)),
                        Ok(None) => {
                            read_failures += 1;
                            last_read_error = Some(error);
                        }
                        Err(cache_error) => {
                            warn_omitted_cache_read(ledger_offset, cursor, &cache_error);
                            read_failures += 1;
                            last_read_error = Some(error);
                        }
                    }
                } else {
                    // The batch path omitted a hole. The missing-body path
                    // already counted its read.
                    if batch.is_some() {
                        note_chunk_read(SERVE_SPAN, "storage_module", "miss");
                    }
                    // A cache fault is this offset only. Single-offset callers
                    // still get the error from `get_chunk_from_cache`.
                    match self.cached_unpacked_at(SERVE_SPAN, &module, partition_offset) {
                        Ok(Some(cached)) => out.push((ledger_offset, cached)),
                        Ok(None) => {}
                        Err(error) => {
                            warn_omitted_cache_read(ledger_offset, cursor, &error);
                        }
                    }
                }
                if cursor == end.0 {
                    break;
                }
                cursor += 1;
            }
        }
        out.sort_by_key(|(offset, _)| *offset);
        if requested_len > 0 && read_failures == requested_len {
            return Err(last_read_error.unwrap_or_else(|| eyre::eyre!("chunk span read failed")));
        }
        Ok(out)
    }

    fn cached_unpacked_at(
        &self,
        caller: &'static str,
        module: &crate::StorageModule,
        partition_offset: PartitionChunkOffset,
    ) -> eyre::Result<Option<UnpackedChunk>> {
        let placement = match module.data_root_and_tx_offset_at(partition_offset) {
            Ok(placement) => placement,
            Err(error) => {
                debug!(
                    error = %error,
                    partition.offset = partition_offset.0,
                    "index lookup failed; omitting offset"
                );
                note_chunk_read(caller, "storage_module", "error");
                return Ok(None);
            }
        };
        let Some((data_commitment, _data_root, tx_offset)) = placement else {
            return Ok(None);
        };
        self.cached_unpacked(caller, data_commitment, tx_offset)
    }

    fn cached_unpacked(
        &self,
        caller: &'static str,
        data_commitment: DataCommitment,
        tx_offset: TxChunkOffset,
    ) -> eyre::Result<Option<UnpackedChunk>> {
        match self.get_chunk_from_cache(data_commitment, tx_offset) {
            Ok(Some(ChunkFormat::Unpacked(chunk))) => {
                note_chunk_read(caller, "cache", "hit");
                Ok(Some(chunk))
            }
            Ok(Some(ChunkFormat::Packed(_)) | None) => {
                note_chunk_read(caller, "cache", "miss");
                Ok(None)
            }
            Err(error) => {
                note_chunk_read(caller, "cache", "error");
                Err(error)
            }
        }
    }

    /// Chunk for `data_root` at a tx-relative offset.
    ///
    /// Data is keyed by the data commitment, and a root alone does not fix
    /// the size. With `data_size`, this is a point lookup of the commitment of
    /// `(data_root, data_size)`. Without it, the lookup scans the data-root
    /// rows of every local storage module for `ledger`, and then the cache
    /// rows, for the commitments whose root is `data_root`. If chunks of more
    /// than one data size hold the offset, it returns [`AmbiguousDataSize`]
    /// and serves none of them.
    ///
    /// Local storage modules for `ledger` are first and return packed bytes.
    /// The node chunk cache is the fallback and returns unpacked bytes, so a
    /// node with no assignment can still serve a body it cached.
    /// `Ok(None)` when the offset is outside `[0, chunk_count)` or the
    /// placement has no body. A stored chunk is returned only when its
    /// `data_root`, `data_size`, and `tx_offset` are the requested placement.
    /// A storage read error is returned when no source holds the chunk.
    pub fn get_chunk_by_data_root(
        &self,
        ledger: DataLedger,
        data_root: DataTreeRoot,
        data_size: Option<u64>,
        data_tx_offset: TxChunkOffset,
    ) -> eyre::Result<Option<ChunkFormat>> {
        index_read_metrics::with_caller(SERVE_DATA_ROOT, || {
            self.data_root_in_caller(ledger, data_root, data_size, data_tx_offset)
        })
    }

    fn data_root_in_caller(
        &self,
        ledger: DataLedger,
        data_root: DataTreeRoot,
        data_size: Option<u64>,
        data_tx_offset: TxChunkOffset,
    ) -> eyre::Result<Option<ChunkFormat>> {
        debug!(
            "getting ledger: {:?}, data_root: {}, data_size: {:?}, offset: {}",
            &ledger, &data_root, &data_size, &data_tx_offset
        );
        let chunk_size = self.config.consensus.chunk_size;
        let num_chunks_in_partition = self.config.consensus.num_chunks_in_partition;
        let lookup = match data_size {
            Some(data_size) => PlacementLookup::Commitment(data_commitment(
                &self.config.data_tree_geometry(),
                data_root,
                data_size,
            )),
            None => PlacementLookup::TreeRoot(data_root),
        };
        // The first chunk found for each data size that holds the offset.
        let mut found: BTreeMap<u64, ChunkFormat> = BTreeMap::new();
        // map hashes to SMs
        let binding = self.storage_modules_guard.read();
        let sms = binding
            .iter()
            .filter(|sm| {
                sm.partition_assignment
                    .read()
                    .unwrap()
                    .and_then(|sm| sm.ledger_id)
                    == Some(ledger as u32)
            })
            .collect::<Vec<_>>();
        // A real read error is not a hole. That includes an index read.
        // A hit still wins. With no hit, the last error is returned.
        let mut disk_error: Option<eyre::Report> = None;

        for sm in sms {
            // One read of this root's placements. `generate_full_chunk` stamps
            // `data_root`, `data_size` and `tx_offset` from the interval that holds
            // the partition offset; those values are checked below.
            let data_root_infos = match read_data_root_placements(sm, lookup) {
                Ok(infos) => infos,
                Err(error) => {
                    debug!(error = %error, "data_root index read failed");
                    disk_error = Some(error);
                    continue;
                }
            };
            for (_, info) in &data_root_infos {
                if found.contains_key(&info.data_size)
                    || !tx_offset_within_data_size(data_tx_offset, info.data_size, chunk_size)
                {
                    continue;
                }
                let Some(partition_offset) = partition_offset_for_placement(
                    info.start_offset,
                    data_tx_offset,
                    num_chunks_in_partition,
                ) else {
                    continue;
                };
                match sm.generate_full_chunk(partition_offset) {
                    Ok(Some(packed)) => {
                        if packed.data_tree_root != data_root
                            || packed.tx_offset != data_tx_offset
                            || packed.data_size != info.data_size
                        {
                            debug!(
                                requested_data_root = %data_root,
                                requested_tx_offset = %data_tx_offset,
                                chunk_data_root = %packed.data_tree_root,
                                chunk_tx_offset = %packed.tx_offset,
                                chunk_data_size = packed.data_size,
                                placement_data_size = info.data_size,
                                %partition_offset,
                                "data_root chunk does not match the requested placement"
                            );
                            continue;
                        }
                        found.insert(info.data_size, ChunkFormat::Packed(packed));
                    }
                    // No body at this placement. A later placement or the cache
                    // may still hold it.
                    Ok(None) => continue,
                    Err(error) if missing_on_disk(&error) => continue,
                    Err(error) => {
                        debug!(
                            error = %error,
                            %partition_offset,
                            "data_root chunk read failed"
                        );
                        disk_error = Some(error);
                    }
                }
            }
        }
        drop(binding);

        // Fall back to the node chunk cache. Non-assignees often never have an SM
        // for this ledger/slot, but may still hold the body after POST /chunk
        // (long enough to generate an ingress proof and to serve residual holes).
        let cached: Vec<(DataCommitment, u64)> = match (lookup, data_size) {
            (PlacementLookup::Commitment(data_commitment), Some(data_size)) => {
                vec![(data_commitment, data_size)]
            }
            _ => self
                .db
                .view_eyre(|tx| cached_data_roots_by_tree_root(tx, data_root))?
                .into_iter()
                .map(|(data_commitment, cdr)| (data_commitment, cdr.data_size))
                .collect(),
        };
        for (data_commitment, cached_data_size) in cached {
            if found.contains_key(&cached_data_size) {
                continue;
            }
            if let Some(chunk) = self.get_chunk_from_cache(data_commitment, data_tx_offset)? {
                found.insert(cached_data_size, chunk);
            }
        }
        if found.len() > 1 {
            return Err(AmbiguousDataSize {
                data_tree_root: data_root,
                tx_offset: data_tx_offset,
                data_sizes: found.into_keys().collect(),
            }
            .into());
        }
        match (found.into_values().next(), disk_error) {
            (Some(chunk), _) => Ok(Some(chunk)),
            (None, Some(error)) => Err(error),
            (None, None) => Ok(None),
        }
    }

    /// Unpacked body from `CachedChunks` / `CachedDataRoots`.
    ///
    /// `Ok(None)` when the data root is missing, the offset is outside
    /// `[0, chunk_count(data_size))`, the offset is not indexed, or the body
    /// was pruned. The response `data_root`, `data_size`, and `tx_offset` are
    /// the index key and `CachedDataRoots`, which is what the count check uses.
    fn get_chunk_from_cache(
        &self,
        data_commitment: DataCommitment,
        data_tx_offset: TxChunkOffset,
    ) -> eyre::Result<Option<ChunkFormat>> {
        #[cfg(test)]
        if CACHE_READ_FAULT.with(std::cell::Cell::get) == Some(*data_tx_offset) {
            return Err(eyre::eyre!("injected cache read error"));
        }
        let chunk_size = self.config.consensus.chunk_size;
        self.db.view_eyre(|tx| {
            let Some(cdr) = cached_data_root_by_commitment(tx, data_commitment)? else {
                return Ok(None);
            };
            if !tx_offset_within_data_size(data_tx_offset, cdr.data_size, chunk_size) {
                debug!(
                    data_commitment = %data_commitment,
                    tx_offset = %data_tx_offset,
                    data_size = cdr.data_size,
                    "data_root offset is past the cached data size"
                );
                return Ok(None);
            }
            let Some((_meta, cached)) =
                cached_chunk_by_chunk_offset(tx, data_commitment, data_tx_offset)?
            else {
                return Ok(None);
            };
            // Index-only rows (body pruned / stored only in a partition) cannot be served.
            let Some(bytes) = cached.chunk else {
                debug!(
                    data_commitment = %data_commitment,
                    tx_offset = %data_tx_offset,
                    "cache index hit but chunk body missing; cannot serve data_root fetch"
                );
                return Ok(None);
            };

            Ok(Some(ChunkFormat::Unpacked(UnpackedChunk {
                data_tree_root: cdr.data_tree_root,
                data_size: cdr.data_size,
                data_path: cached.data_path,
                bytes,
                tx_offset: data_tx_offset,
            })))
        })
    }

    /// Ledger start offsets of the placements of `data_commitment`, from the
    /// first local storage module that has one.
    pub fn get_ledger_offsets_for_commitment(
        &self,
        ledger: DataLedger,
        data_commitment: DataCommitment,
    ) -> eyre::Result<Option<Vec<u64>>> {
        debug!(
            "getting ledger: {:?}, data_commitment: {}",
            &ledger, &data_commitment
        );

        // get all SMs for this ledger
        let binding = self.storage_modules_guard.read();
        let sms = binding
            .iter()
            .filter(|sm| {
                sm.partition_assignment
                    .read()
                    .unwrap()
                    .and_then(|sm| sm.ledger_id)
                    == Some(ledger as u32)
            })
            .collect::<Vec<_>>();

        // The commitment binds the data size, so every placement found here has it.

        // An index error is not proof the root is absent. A later module may
        // still hold it. Return that error only when none does. A module with
        // no ledger range cannot be translated, so it is skipped.
        let mut disk_error: Option<eyre::Report> = None;
        // find a SM that contains this commitment, return the start_offsets once we find it
        for sm in sms {
            let sm_range_start = match sm.get_storage_module_ledger_offsets() {
                Ok(range) => range.start(),
                Err(error) => {
                    debug!(
                        error = %error,
                        "storage module has no ledger range; skipping it"
                    );
                    continue;
                }
            };
            let data_root_infos =
                match read_data_root_placements(sm, PlacementLookup::Commitment(data_commitment)) {
                    Ok(infos) => infos,
                    Err(error) => {
                        debug!(error = %error, "data_root index read failed");
                        disk_error = Some(error);
                        continue;
                    }
                };
            let mapped_offsets = data_root_infos
                .iter()
                .filter_map(|(_, info)| {
                    checked_add_i32_u64(*info.start_offset, sm_range_start.into())
                    // translate into ledger-relative space
                })
                .collect::<Vec<_>>();

            if !mapped_offsets.is_empty() {
                return Ok(Some(mapped_offsets));
            }
        }
        match disk_error {
            Some(error) => Err(error),
            None => Ok(None),
        }
    }
}

/// `data_tx_offset` is inside `[0, chunk_count(data_size))`.
///
/// `data_size == 0` and a chunk count that does not fit in `u32` are out of range.
fn tx_offset_within_data_size(
    data_tx_offset: TxChunkOffset,
    data_size: u64,
    chunk_size: u64,
) -> bool {
    let Ok(chunk_count) = data_size_to_chunk_count(data_size, chunk_size) else {
        return false;
    };
    *data_tx_offset < chunk_count
}

/// Partition offset of one placement, or `None` when that sum falls outside the module.
///
/// `start_offset` is negative when the transaction began in an earlier partition.
fn partition_offset_for_placement(
    start_offset: RelativeChunkOffset,
    data_tx_offset: TxChunkOffset,
    num_chunks_in_partition: u64,
) -> Option<PartitionChunkOffset> {
    let relative = i64::from(start_offset.0) + i64::from(*data_tx_offset);
    let relative = u64::try_from(relative).ok()?;
    if relative >= num_chunks_in_partition {
        return None;
    }
    u32::try_from(relative).ok().map(PartitionChunkOffset::from)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use crate::{StorageModule, StorageModuleInfo};

    use super::*;
    use irys_database::{cache_chunk, cache_data_root};
    use irys_packing::unpack_with_entropy;
    use irys_testing_utils::utils::TempDirBuilder;
    use irys_types::SubmoduleIndexEngine;
    use irys_types::{
        Base64, Config, ConsensusConfig, DataTransaction, DataTransactionHeader,
        DataTransactionLedger, H256, LedgerChunkOffset, LedgerChunkRange, NodeConfig,
        PartitionChunkOffset, UnpackedChunk, irys::IrysSigner, ledger_chunk_offset_ii,
        partition::PartitionAssignment, partition_chunk_offset_ie,
    };
    use nodit::interval::ii;
    use rand::Rng as _;
    use rstest::rstest;

    #[test]
    fn missing_on_disk_matches_only_the_typed_hole() {
        let hole = eyre::Report::from(ChunkMissingOnDisk);
        assert!(missing_on_disk(&hole));
        assert!(missing_on_disk(&hole.wrap_err("outer context")));
        assert!(!missing_on_disk(&eyre::eyre!("some other failure")));
    }

    #[test]
    // MDBX-SUBMODULE-INDEX: this test opens only MDBX. Retarget it at RocksDB.
    fn get_by_data_tx_offset_test() -> eyre::Result<()> {
        let (tmp_dir, config, storage_module) =
            opened_publish("get_by_data_tx_offset_test", SubmoduleIndexEngine::Mdbx)?;
        let data_size = (config.consensus.chunk_size as f64 * 2.5).round() as usize;
        let mut data_bytes = vec![0_u8; data_size];
        rand::thread_rng().fill(&mut data_bytes[..]);
        let tx = signed_tx(&config, data_bytes.clone());
        index_placement(&storage_module, &tx, 49)?;
        let chunks = tx.data_chunks().unwrap();
        write_chunks(&storage_module, &chunks)?;
        storage_module.sync_pending_chunks()?;
        let provider = provider(
            config.clone(),
            storage_module,
            &tmp_dir.path().join("irys_db"),
        );
        for chunk in &chunks {
            assert_serves_packed(&provider, &config, chunk)?;
        }
        Ok(())
    }

    /// A node with no storage module still serves a cached body by data root.
    #[test]
    // MDBX-SUBMODULE-INDEX: this test opens only MDBX. Retarget it at RocksDB.
    fn get_by_data_root_serves_from_chunk_cache_without_sm() -> eyre::Result<()> {
        let (tmp_dir, config) =
            opened_config("get_by_data_root_cache_test", SubmoduleIndexEngine::Mdbx);
        let data_size = (config.consensus.chunk_size as f64 * 2.5).round() as usize;
        let mut data_bytes = vec![0_u8; data_size];
        rand::thread_rng().fill(&mut data_bytes[..]);
        let tx = signed_tx(&config, data_bytes.clone());
        let chunks = tx.data_chunks().unwrap();
        let data_root = tx.header.data_tree_root;
        let db = create_test_db(&tmp_dir.path().join("irys_db"));
        cache_chunks(&db, &config, &tx, &chunks);
        let chunk_provider = make_provider(config, Vec::new(), db);

        for expected in &chunks {
            let served = chunk_provider
                .get_chunk_by_data_root(DataLedger::Submit, data_root, None, expected.tx_offset)?
                .expect("cache-backed data_root fetch should find body");
            let unpacked = served
                .as_unpacked()
                .expect("cache path returns ChunkFormat::Unpacked");
            assert_eq!(unpacked, *expected);
        }

        let missing = TxChunkOffset::from(99_u32);
        assert!(
            chunk_provider
                .get_chunk_by_data_root(DataLedger::Submit, data_root, None, missing)?
                .is_none()
        );
        Ok(())
    }

    /// The data size of a served chunk.
    fn served_size(chunk: &ChunkFormat) -> u64 {
        match chunk {
            ChunkFormat::Packed(packed) => packed.data_size,
            ChunkFormat::Unpacked(unpacked) => unpacked.data_size,
        }
    }

    /// The data size served at `offset`, `None` for a miss, or the sizes of
    /// an ambiguous root-only lookup.
    fn lookup_size(
        provider: &ChunkProvider,
        ledger: DataLedger,
        data_root: DataTreeRoot,
        data_size: Option<u64>,
        offset: u32,
    ) -> eyre::Result<Result<Option<u64>, Vec<u64>>> {
        match provider.get_chunk_by_data_root(
            ledger,
            data_root,
            data_size,
            TxChunkOffset::from(offset),
        ) {
            Ok(chunk) => Ok(Ok(chunk.as_ref().map(served_size))),
            Err(error) => match error.downcast_ref::<AmbiguousDataSize>() {
                Some(ambiguous) => {
                    assert_eq!(ambiguous.data_tree_root, data_root);
                    assert_eq!(ambiguous.tx_offset, TxChunkOffset::from(offset));
                    Ok(Err(ambiguous.data_sizes.clone()))
                }
                None => Err(error),
            },
        }
    }

    /// Two sizes of one data tree root are two data commitments. From the
    /// cache: with `data_size` each size is a point lookup; without it, an
    /// offset that both sizes hold is ambiguous and one that only one size
    /// holds is served from that size.
    #[test]
    // MDBX-SUBMODULE-INDEX: this test opens only MDBX. Retarget it at RocksDB.
    fn get_by_data_root_from_the_cache_with_two_sizes_of_one_root() -> eyre::Result<()> {
        let (tmp_dir, config) =
            opened_config("get_by_data_root_two_sizes", SubmoduleIndexEngine::Mdbx);
        let chunk_size = config.consensus.chunk_size as usize;
        let bytes = patterned_bytes(0x21, chunk_size * 2);
        let short_tx = signed_tx(&config, bytes);
        let short_chunks = short_tx.data_chunks().unwrap();
        let data_root = short_tx.header.data_tree_root;
        let short_size = short_tx.header.data_size;

        // Same root, one more chunk: a different commitment. Offsets 0 and 1
        // carry the same data_path as the short upload.
        let mut long_header = short_tx.header.clone();
        long_header.id = H256::random();
        long_header.data_size = (chunk_size * 3) as u64;
        let long_size = long_header.data_size;
        let mut long_chunks: Vec<UnpackedChunk> = short_chunks
            .iter()
            .cloned()
            .map(|chunk| UnpackedChunk {
                data_size: long_size,
                ..chunk
            })
            .collect();
        let tail = UnpackedChunk {
            data_tree_root: data_root,
            data_size: long_size,
            data_path: Base64(vec![0x77; 4]),
            bytes: Base64(vec![0x5A; chunk_size]),
            tx_offset: TxChunkOffset::from(2_u32),
        };
        long_chunks.push(tail.clone());
        let geometry = config.data_tree_geometry();
        assert_ne!(
            short_tx.header.data_commitment(&geometry),
            long_header.data_commitment(&geometry)
        );

        let db = create_test_db(&tmp_dir.path().join("irys_db"));
        cache_chunks(&db, &config, &short_tx, &short_chunks);
        db.update_eyre(|wtx| {
            cache_data_root(wtx, &long_header, None, &geometry)?;
            for chunk in &long_chunks {
                cache_chunk(wtx, chunk, &geometry)?;
            }
            Ok(())
        })?;
        let provider = make_provider(config, Vec::new(), db);
        let ledger = DataLedger::Submit;

        // An offset both sizes hold.
        assert_eq!(
            lookup_size(&provider, ledger, data_root, None, 1)?,
            Err(vec![short_size, long_size])
        );
        assert_eq!(
            lookup_size(&provider, ledger, data_root, Some(short_size), 1)?,
            Ok(Some(short_size))
        );
        assert_eq!(
            lookup_size(&provider, ledger, data_root, Some(long_size), 1)?,
            Ok(Some(long_size))
        );
        let served = provider
            .get_chunk_by_data_root(
                ledger,
                data_root,
                Some(short_size),
                TxChunkOffset::from(1_u32),
            )?
            .expect("the short upload holds offset 1");
        assert_eq!(
            served.as_unpacked().expect("cache serves unpacked"),
            short_chunks[1]
        );

        // An offset only the long size holds.
        assert_eq!(
            lookup_size(&provider, ledger, data_root, None, 2)?,
            Ok(Some(long_size))
        );
        assert_eq!(
            lookup_size(&provider, ledger, data_root, Some(long_size), 2)?,
            Ok(Some(long_size))
        );
        assert_eq!(
            lookup_size(&provider, ledger, data_root, Some(short_size), 2)?,
            Ok(None)
        );
        let served = provider
            .get_chunk_by_data_root(ledger, data_root, None, TxChunkOffset::from(2_u32))?
            .expect("the long upload holds offset 2");
        assert_eq!(served.as_unpacked().expect("cache serves unpacked"), tail);

        // No size holds offset 3.
        assert_eq!(
            lookup_size(&provider, ledger, data_root, None, 3)?,
            Ok(None)
        );
        Ok(())
    }

    /// Two sizes of one data tree root placed in a storage module. With
    /// `data_size` each size is a point lookup of its own placement; without
    /// it, an offset that both placements hold is ambiguous and one that only
    /// the longer placement holds is served from it.
    #[rstest]
    // MDBX-SUBMODULE-INDEX: delete this MDBX case and keep the RocksDB case.
    #[case::mdbx(SubmoduleIndexEngine::Mdbx)]
    #[case::rocksdb(SubmoduleIndexEngine::Rocksdb)]
    fn get_by_data_root_from_storage_with_two_sizes_of_one_root(
        #[case] engine: SubmoduleIndexEngine,
    ) -> eyre::Result<()> {
        let (tmp_dir, config, storage_module) = opened_publish("data_root_two_sizes_sm", engine)?;
        let chunk_size = config.consensus.chunk_size as usize;

        // The real tree has three chunks. The short header claims the same
        // root with two chunks; its chunks 0 and 1 carry the same data_path.
        let bytes = patterned_bytes(0x63, chunk_size * 3);
        let long_tx = signed_tx(&config, bytes);
        let long_chunks = long_tx.data_chunks().unwrap();
        let data_root = long_tx.header.data_tree_root;
        let long_size = long_tx.header.data_size;
        let mut short_header = long_tx.header.clone();
        short_header.id = H256::random();
        short_header.data_size = (chunk_size * 2) as u64;
        let short_size = short_header.data_size;
        let short_chunks: Vec<UnpackedChunk> = long_chunks[..2]
            .iter()
            .cloned()
            .map(|chunk| UnpackedChunk {
                data_size: short_size,
                ..chunk
            })
            .collect();

        let index_header = |header: &DataTransactionHeader, ledger_start: u64, chunks: u64| {
            let (_tx_root, proofs) =
                DataTransactionLedger::merklize_tx_root(std::slice::from_ref(header));
            let range = LedgerChunkRange(ledger_chunk_offset_ii!(
                ledger_start,
                ledger_start + chunks - 1
            ));
            storage_module.index_transaction_data(header, &proofs[0].proof, range, 0)
        };
        index_header(&long_tx.header, 10, 3)?;
        index_header(&short_header, 20, 2)?;
        write_chunks(&storage_module, &long_chunks)?;
        write_chunks(&storage_module, &short_chunks)?;
        storage_module.sync_pending_chunks()?;

        let provider = provider(config, storage_module, &tmp_dir.path().join("irys_db"));
        let ledger = DataLedger::Publish;

        // An offset both placements hold.
        assert_eq!(
            lookup_size(&provider, ledger, data_root, None, 1)?,
            Err(vec![short_size, long_size])
        );
        assert_eq!(
            lookup_size(&provider, ledger, data_root, Some(short_size), 1)?,
            Ok(Some(short_size))
        );
        assert_eq!(
            lookup_size(&provider, ledger, data_root, Some(long_size), 1)?,
            Ok(Some(long_size))
        );

        // An offset only the long placement holds.
        assert_eq!(
            lookup_size(&provider, ledger, data_root, None, 2)?,
            Ok(Some(long_size))
        );
        assert_eq!(
            lookup_size(&provider, ledger, data_root, Some(long_size), 2)?,
            Ok(Some(long_size))
        );
        assert_eq!(
            lookup_size(&provider, ledger, data_root, Some(short_size), 2)?,
            Ok(None)
        );
        Ok(())
    }

    /// Two adjacent placements. `data_tx_offset == chunk_count` of the first tx
    /// is the first byte of the next tx. That offset must not be served.
    #[rstest]
    // MDBX-SUBMODULE-INDEX: delete this MDBX case and keep the RocksDB case.
    #[case::mdbx(SubmoduleIndexEngine::Mdbx)]
    #[case::rocksdb(SubmoduleIndexEngine::Rocksdb)]
    fn out_of_range_data_tx_offset_does_not_serve_the_next_placement(
        #[case] engine: SubmoduleIndexEngine,
    ) -> eyre::Result<()> {
        let (tmp_dir, config, storage_module) = opened_publish("data_root_next_placement", engine)?;
        let chunk_size = config.consensus.chunk_size as usize;

        let bytes_a = patterned_bytes(0xA1, chunk_size * 2);
        let bytes_b = patterned_bytes(0xB2, chunk_size * 2);
        let tx_a = signed_tx(&config, bytes_a.clone());
        let tx_b = signed_tx(&config, bytes_b);
        assert_ne!(tx_a.header.data_tree_root, tx_b.header.data_tree_root);
        assert_eq!(tx_a.data_chunks().unwrap().len(), 2);
        assert_eq!(tx_a.header.data_size, bytes_a.len() as u64);

        // First tx occupies ledger offsets 10..=11. The next tx starts at 12,
        // which is `start_offset + chunk_count` for the first tx.
        index_placement(&storage_module, &tx_a, 10)?;
        index_placement(&storage_module, &tx_b, 12)?;
        let chunks_a = tx_a.data_chunks().unwrap();
        let chunks_b = tx_b.data_chunks().unwrap();
        write_chunks(&storage_module, &chunks_a)?;
        write_chunks(&storage_module, &chunks_b)?;
        storage_module.sync_pending_chunks()?;

        let provider = provider(
            config.clone(),
            storage_module,
            &tmp_dir.path().join("irys_db"),
        );

        // Last valid chunk, and the neighbor's first chunk, are both stored.
        assert_serves_packed(&provider, &config, &chunks_a[1])?;
        assert_serves_packed(&provider, &config, &chunks_b[0])?;
        // First invalid offset of the first tx must not return the neighbor.
        assert_offset_absent(&provider, tx_a.header.data_tree_root, 2)?;

        Ok(())
    }

    /// `data_size` is not a multiple of `chunk_size`. The short final chunk is
    /// in range. The next offset is the following placement.
    #[rstest]
    // MDBX-SUBMODULE-INDEX: delete this MDBX case and keep the RocksDB case.
    #[case::mdbx(SubmoduleIndexEngine::Mdbx)]
    #[case::rocksdb(SubmoduleIndexEngine::Rocksdb)]
    fn partial_final_chunk_is_in_range_and_the_next_offset_is_not(
        #[case] engine: SubmoduleIndexEngine,
    ) -> eyre::Result<()> {
        let (tmp_dir, config, storage_module) = opened_publish("data_root_partial_chunk", engine)?;
        let chunk_size = config.consensus.chunk_size as usize;
        let short_len = chunk_size / 2;
        let bytes_a = patterned_bytes(0x11, chunk_size * 2 + short_len);
        let bytes_b = patterned_bytes(0x22, chunk_size);
        let tx_a = signed_tx(&config, bytes_a.clone());
        let tx_b = signed_tx(&config, bytes_b);
        assert_eq!(tx_a.data_chunks().unwrap().len(), 3);
        assert_eq!(tx_a.header.data_size, bytes_a.len() as u64);

        // Offsets 20..=22 are the three chunks of A (the last is short).
        // Offset 23 is B, which `start + chunk_count` names.
        index_placement(&storage_module, &tx_a, 20)?;
        index_placement(&storage_module, &tx_b, 23)?;
        let chunks_a = tx_a.data_chunks().unwrap();
        let chunks_b = tx_b.data_chunks().unwrap();
        assert_eq!(chunks_a[2].bytes.0.len(), short_len);
        write_chunks(&storage_module, &chunks_a)?;
        write_chunks(&storage_module, &chunks_b)?;
        storage_module.sync_pending_chunks()?;

        let provider = provider(
            config.clone(),
            storage_module,
            &tmp_dir.path().join("irys_db"),
        );

        assert_serves_packed(&provider, &config, &chunks_a[2])?;
        assert_serves_packed(&provider, &config, &chunks_b[0])?;
        assert_offset_absent(&provider, tx_a.header.data_tree_root, 3)?;

        Ok(())
    }

    /// The same data root stored twice, back to back. An offset past
    /// `chunk_count` names the second placement's first chunk and must not
    /// be returned as if it were that offset.
    #[rstest]
    // MDBX-SUBMODULE-INDEX: delete this MDBX case and keep the RocksDB case.
    #[case::mdbx(SubmoduleIndexEngine::Mdbx)]
    #[case::rocksdb(SubmoduleIndexEngine::Rocksdb)]
    fn same_data_root_second_placement_does_not_extend_chunk_count(
        #[case] engine: SubmoduleIndexEngine,
    ) -> eyre::Result<()> {
        let (tmp_dir, config, storage_module) = opened_publish("data_root_two_placements", engine)?;
        let chunk_size = config.consensus.chunk_size as usize;

        let bytes = patterned_bytes(0x42, chunk_size * 2);
        let tx = signed_tx(&config, bytes);
        assert_eq!(tx.data_chunks().unwrap().len(), 2);

        // Placements at 30..=31 and 32..=33. Offset 2 of the first placement
        // is partition offset 32, the second placement's chunk 0.
        index_placement(&storage_module, &tx, 30)?;
        index_placement(&storage_module, &tx, 32)?;
        let chunks = tx.data_chunks().unwrap();
        write_chunks(&storage_module, &chunks)?;
        storage_module.sync_pending_chunks()?;

        let data_root = tx.header.data_tree_root;
        let provider = provider(
            config.clone(),
            storage_module,
            &tmp_dir.path().join("irys_db"),
        );

        assert_serves_packed(&provider, &config, &chunks[0])?;
        assert_serves_packed(&provider, &config, &chunks[1])?;
        let second_placement = provider
            .get_chunk_by_ledger_offset(DataLedger::Publish, LedgerChunkOffset::from(32_u64))?
            .ok_or_else(|| eyre::eyre!("second placement was not stored"))?;
        assert_eq!(second_placement.data_tree_root, data_root);
        assert_eq!(second_placement.tx_offset, TxChunkOffset::from(0_u32));
        assert_offset_absent(&provider, data_root, 2)?;

        Ok(())
    }

    /// A cached body stored past `chunk_count(data_size)` is not served.
    /// The cache key is `(data_root, tx_offset)`, so this does not alias into
    /// another tx; the bound is the data size on `CachedDataRoots`.
    #[test]
    // MDBX-SUBMODULE-INDEX: this test opens only MDBX. Retarget it at RocksDB.
    fn cache_does_not_serve_offset_past_data_size() -> eyre::Result<()> {
        let (tmp_dir, config) = opened_config("data_root_cache_bounds", SubmoduleIndexEngine::Mdbx);
        let chunk_size = config.consensus.chunk_size as usize;
        let bytes = patterned_bytes(0x7C, chunk_size * 2);
        let tx = signed_tx(&config, bytes.clone());
        assert_eq!(tx.data_chunks().unwrap().len(), 2);
        let data_root = tx.header.data_tree_root;
        let chunks = tx.data_chunks().unwrap();
        let past_end = TxChunkOffset::from(2_u32);

        let db = create_test_db(&tmp_dir.path().join("irys_db"));
        cache_chunks(&db, &config, &tx, &chunks);
        // Distinct data_path so the cache does not treat this as a duplicate
        // of a real chunk and skip the out-of-range index entry.
        db.update_eyre(|wtx| {
            cache_chunk(
                wtx,
                &UnpackedChunk {
                    data_tree_root: data_root,
                    data_size: bytes.len() as u64,
                    data_path: Base64(vec![0xEE, 0xEE, 0xEE, past_end.0 as u8]),
                    bytes: Base64(vec![0xFF; 4]),
                    tx_offset: past_end,
                },
                &config.data_tree_geometry(),
            )?;
            Ok(())
        })?;
        let chunk_provider = make_provider(config, Vec::new(), db);

        let last = chunk_provider
            .get_chunk_by_data_root(DataLedger::Submit, data_root, None, chunks[1].tx_offset)?
            .ok_or_else(|| eyre::eyre!("last valid cached chunk missing"))?;
        assert_eq!(
            last.as_unpacked().expect("cache returns unpacked"),
            chunks[1]
        );

        let served =
            chunk_provider.get_chunk_by_data_root(DataLedger::Submit, data_root, None, past_end)?;
        if let Some(chunk) = served {
            eyre::bail!("cached offset {past_end} past data_size was served: {chunk:?}");
        }

        Ok(())
    }

    fn opened_config(
        prefix: &str,
        engine: SubmoduleIndexEngine,
    ) -> (irys_testing_utils::utils::tempfile::TempDir, Config) {
        let tmp = TempDirBuilder::new().prefix(prefix).with_tracing().build();
        let config = bounds_testing_config(tmp.path().to_path_buf(), engine);
        (tmp, config)
    }

    fn opened_publish(
        prefix: &str,
        engine: SubmoduleIndexEngine,
    ) -> eyre::Result<(
        irys_testing_utils::utils::tempfile::TempDir,
        Config,
        StorageModule,
    )> {
        let (tmp, config) = opened_config(prefix, engine);
        let storage_module = testing_publish_module(&config)?;
        Ok((tmp, config, storage_module))
    }

    fn opened_span(
        prefix: &str,
        chunks: u64,
    ) -> (irys_testing_utils::utils::tempfile::TempDir, Config) {
        let tmp = TempDirBuilder::new().prefix(prefix).with_tracing().build();
        let config = testing_config(tmp.path().to_path_buf(), chunks);
        (tmp, config)
    }

    fn bounds_testing_config(
        base_path: std::path::PathBuf,
        engine: SubmoduleIndexEngine,
    ) -> Config {
        let testing = NodeConfig::testing();
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: 100,
                ..ConsensusConfig::testing()
            }),
            base_directory: base_path,
            database: irys_types::DatabaseConfig {
                submodule_index: engine,
                ..testing.database
            },
            ..testing
        };
        Config::new_with_random_peer_id(node_config)
    }

    fn testing_publish_module(config: &Config) -> eyre::Result<StorageModule> {
        let info = StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment::default()),
            submodules: vec![
                (partition_chunk_offset_ie!(0, 50), "hdd0".into()),
                (partition_chunk_offset_ie!(50, 100), "hdd1".into()),
            ],
        };
        let storage_module = StorageModule::new(&info, config)?;
        storage_module.pack_with_zeros();
        Ok(storage_module)
    }

    fn patterned_bytes(tag: u8, len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| tag.wrapping_add((i % 251) as u8))
            .collect()
    }

    fn signed_tx(config: &Config, bytes: Vec<u8>) -> irys_types::DataTransaction {
        let irys = IrysSigner::random_signer(&config.consensus);
        let tx = irys.create_transaction(bytes, H256::zero()).unwrap();
        irys.sign_transaction(tx).unwrap()
    }

    fn index_placement(
        storage_module: &StorageModule,
        tx: &irys_types::DataTransaction,
        ledger_start: u64,
    ) -> eyre::Result<()> {
        let (_tx_root, proofs) =
            DataTransactionLedger::merklize_tx_root(std::slice::from_ref(&tx.header));
        let chunk_count = tx.data_chunks().unwrap().len() as u64;
        let range = LedgerChunkRange(ledger_chunk_offset_ii!(
            ledger_start,
            ledger_start + chunk_count - 1
        ));
        storage_module.index_transaction_data(&tx.header, &proofs[0].proof, range, 0)
    }

    fn write_chunks(storage_module: &StorageModule, chunks: &[UnpackedChunk]) -> eyre::Result<()> {
        for chunk in chunks {
            storage_module.write_data_chunk(chunk)?;
        }
        Ok(())
    }

    fn provider(
        config: Config,
        storage_module: StorageModule,
        db_path: &std::path::Path,
    ) -> ChunkProvider {
        make_provider(
            config,
            vec![Arc::new(storage_module)],
            create_test_db(db_path),
        )
    }

    fn assert_serves_packed(
        provider: &ChunkProvider,
        config: &Config,
        expected: &UnpackedChunk,
    ) -> eyre::Result<()> {
        let Some(chunk) = provider.get_chunk_by_data_root(
            DataLedger::Publish,
            DataTreeRoot(expected.data_tree_root.0),
            None,
            expected.tx_offset,
        )?
        else {
            eyre::bail!("missing chunk at tx offset {}", expected.tx_offset);
        };
        let packed = chunk
            .as_packed()
            .ok_or_else(|| eyre::eyre!("storage module path must return a packed chunk"))?;
        let unpacked_data = unpack_with_entropy(
            &packed,
            vec![0_u8; config.consensus.chunk_size as usize],
            config.consensus.chunk_size as usize,
        )?;
        let unpacked = UnpackedChunk {
            data_tree_root: packed.data_tree_root,
            data_size: packed.data_size,
            data_path: packed.data_path.clone(),
            bytes: Base64(unpacked_data),
            tx_offset: packed.tx_offset,
        };
        assert_eq!(&unpacked, expected);
        Ok(())
    }

    fn assert_offset_absent(
        provider: &ChunkProvider,
        data_root: DataTreeRoot,
        offset: u32,
    ) -> eyre::Result<()> {
        let served = provider.get_chunk_by_data_root(
            DataLedger::Publish,
            data_root,
            None,
            TxChunkOffset::from(offset),
        )?;
        if let Some(chunk) = served {
            eyre::bail!("tx offset {offset} of {data_root} was served: {chunk:?}");
        }
        Ok(())
    }

    fn create_test_db(path: &std::path::Path) -> DatabaseProvider {
        use irys_database::{IrysDatabaseArgs as _, open_or_create_db, tables::IrysTables};
        use reth_db::mdbx::DatabaseArguments;

        let db = open_or_create_db(
            path,
            IrysTables::ALL,
            DatabaseArguments::irys_testing().unwrap(),
        )
        .unwrap();
        DatabaseProvider(Arc::new(db))
    }

    fn testing_config(base: std::path::PathBuf, num_chunks: u64) -> Config {
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: num_chunks,
                entropy_packing_iterations: 1,
                ..ConsensusConfig::testing()
            }),
            base_directory: base,
            ..NodeConfig::testing()
        };
        Config::new_with_random_peer_id(node_config)
    }

    fn one_chunk(config: &Config, byte: u8) -> (DataTransaction, Vec<u8>) {
        let data = vec![byte; config.consensus.chunk_size as usize];
        let signer = IrysSigner::random_signer(&config.consensus);
        let tx = signer
            .create_transaction(data.clone(), H256::zero())
            .unwrap();
        let tx = signer.sign_transaction(tx).unwrap();
        (tx, data)
    }

    fn index_one(sm: &StorageModule, tx: &DataTransaction, ledger_offset: u64) {
        let (_root, proofs) =
            DataTransactionLedger::merklize_tx_root(std::slice::from_ref(&tx.header));
        sm.index_transaction_data(
            &tx.header,
            &proofs[0].proof,
            LedgerChunkRange(ledger_chunk_offset_ii!(ledger_offset, ledger_offset)),
            0,
        )
        .unwrap();
    }

    /// Name the chunk in the index without writing its bytes. A one-offset read
    /// then errors instead of reporting a hole.
    fn remember_data_path(
        sm: &StorageModule,
        partition_offset: PartitionChunkOffset,
        data_path: &Vec<u8>,
    ) {
        use irys_database::submodule::SubmoduleStore as _;

        let path_hash = UnpackedChunk::hash_data_path(data_path);
        let (_, submodule) = sm.get_submodule_for_offset(partition_offset).unwrap();
        submodule
            .db
            .update(|tx| {
                tx.write_data_path_updates(vec![(partition_offset, path_hash, data_path.clone())])
            })
            .unwrap();
    }

    fn cache_chunks(
        db: &DatabaseProvider,
        config: &Config,
        tx: &DataTransaction,
        chunks: &[UnpackedChunk],
    ) {
        let geometry = config.data_tree_geometry();
        db.update_eyre(|wtx| {
            cache_data_root(wtx, &tx.header, None, &geometry)?;
            for chunk in chunks {
                cache_chunk(wtx, chunk, &geometry)?;
            }
            Ok(())
        })
        .unwrap();
    }

    fn cache_one(db: &DatabaseProvider, config: &Config, tx: &DataTransaction, data: &[u8]) {
        cache_chunks(
            db,
            config,
            tx,
            &[UnpackedChunk {
                data_tree_root: tx.header.data_tree_root,
                data_size: tx.header.data_size,
                data_path: tx.chunk_proof(TxChunkOffset::from(0_u32)).unwrap(),
                bytes: Base64(data.to_vec()),
                tx_offset: TxChunkOffset::from(0_u32),
            }],
        );
    }

    fn pack_entropy(sm: &StorageModule, config: &Config, offset: u32) {
        let chunk_size = config.consensus.chunk_size as usize;
        let partition_hash = sm.partition_hash().expect("assigned partition");
        let mut entropy = Vec::with_capacity(chunk_size);
        irys_packing::capacity_single::compute_entropy_chunk(
            config.node_config.miner_address(),
            u64::from(offset),
            partition_hash.0,
            config.consensus.entropy_packing_iterations,
            chunk_size,
            &mut entropy,
            config.consensus.chain_id,
        );
        assert!(sm.write_chunk(
            PartitionChunkOffset::from(offset),
            entropy,
            crate::models::ChunkType::Entropy,
        ));
        sm.sync_pending_chunks().expect("entropy durable");
    }

    fn assigned(
        config: &Config,
        id: usize,
        slot: usize,
        name: &str,
        num_chunks: u32,
    ) -> StorageModule {
        let assignment = PartitionAssignment {
            slot_index: Some(slot),
            partition_hash: H256::repeat_byte(u8::try_from(slot).unwrap_or(1) + 1),
            ..PartitionAssignment::default()
        };
        let info = StorageModuleInfo {
            id,
            partition_assignment: Some(assignment),
            submodules: vec![(partition_chunk_offset_ie!(0, num_chunks), name.into())],
        };
        StorageModule::new(&info, config).unwrap()
    }

    fn make_provider(
        config: Config,
        modules: Vec<Arc<StorageModule>>,
        db: DatabaseProvider,
    ) -> ChunkProvider {
        let guard = StorageModulesReadGuard::new(Arc::new(RwLock::new(modules)));
        ChunkProvider::new(config, guard, db)
    }

    #[test]
    fn ledger_offset_without_a_storage_module_is_absent() {
        let (tmp, config) = opened_span("ledger-no-sm", 4);
        let db = create_test_db(&tmp.path().join("db"));
        let provider = make_provider(config, Vec::new(), db);

        let chunk = provider
            .get_chunk_by_ledger_offset(DataLedger::Publish, LedgerChunkOffset::from(0))
            .unwrap();
        assert!(chunk.is_none());
    }

    #[test]
    fn span_uses_the_cache_when_the_module_has_no_body() {
        let (tmp, config) = opened_span("ledger-cache-fallback", 4);
        let sm = assigned(&config, 0, 0, "hdd0", 4);
        let (tx, data) = one_chunk(&config, 7);
        index_one(&sm, &tx, 0);
        let sm = Arc::new(sm);

        let db = create_test_db(&tmp.path().join("db"));
        cache_one(&db, &config, &tx, &data);
        let provider = make_provider(config.clone(), vec![Arc::clone(&sm)], db);
        assert!(
            provider
                .get_chunk_by_ledger_offset(DataLedger::Publish, LedgerChunkOffset::from(0))
                .unwrap()
                .is_none(),
            "the ledger route does not pack a cached body"
        );

        let span = provider
            .get_unpacked_chunks_by_ledger_span(
                DataLedger::Publish,
                LedgerChunkOffset::from(0),
                LedgerChunkOffset::from(0),
            )
            .unwrap();
        assert_eq!(span.len(), 1);
        assert_eq!(span[0].1.bytes.0, data);

        let empty = create_test_db(&tmp.path().join("empty"));
        let provider = make_provider(config, vec![sm], empty);
        assert!(
            provider
                .get_chunk_by_ledger_offset(DataLedger::Publish, LedgerChunkOffset::from(0))
                .unwrap()
                .is_none()
        );
        assert!(
            provider
                .get_unpacked_chunks_by_ledger_span(
                    DataLedger::Publish,
                    LedgerChunkOffset::from(0),
                    LedgerChunkOffset::from(0),
                )
                .unwrap()
                .is_empty()
        );
    }

    /// An indexed offset with no body is a hole. The span omits it and
    /// counts a miss. A failed read stays an error.
    #[test]
    fn span_hole_is_a_miss() {
        let (tmp, config) = opened_span("span-hole-miss", 4);
        let sm = assigned(&config, 0, 0, "hdd0", 4);
        let (tx, _data) = one_chunk(&config, 3);
        index_one(&sm, &tx, 0);
        let offset = PartitionChunkOffset(0);
        remember_data_path(
            &sm,
            offset,
            &tx.chunk_proof(TxChunkOffset::from(0_u32)).unwrap().0,
        );
        let err = sm
            .generate_full_chunks(offset, offset)
            .expect_err("indexed chunk with no bytes");
        assert!(
            err.to_string()
                .contains("Could not find chunk bytes on disk"),
            "{err}"
        );
        assert!(missing_on_disk(&err), "{err}");

        let db = create_test_db(&tmp.path().join("db"));
        let provider = make_provider(config, vec![Arc::new(sm)], db);
        let _ = take_chunk_read_notes();
        let span = provider
            .get_unpacked_chunks_by_ledger_span(
                DataLedger::Publish,
                LedgerChunkOffset::from(0),
                LedgerChunkOffset::from(0),
            )
            .expect("a hole is absent, not a failed span");
        assert!(span.is_empty(), "{span:?}");
        let notes = take_chunk_read_notes();
        assert!(
            notes.contains(&("serve_span", "storage_module", "miss")),
            "a span hole was not a miss: {notes:?}"
        );
        assert!(
            !notes.contains(&("serve_span", "storage_module", "error")),
            "a span hole was counted as a read error: {notes:?}"
        );
    }

    #[test]
    fn span_skips_a_module_error_and_keeps_the_other_chunk() {
        let (tmp, config) = opened_span("span-skip-module-error", 4);
        let (hole_tx, hole_data) = one_chunk(&config, 3);
        let (good_tx, good_data) = one_chunk(&config, 9);

        let hole = assigned(&config, 0, 0, "hdd0", 4);
        index_one(&hole, &hole_tx, 3);
        let hole_offset = PartitionChunkOffset(3);
        remember_data_path(
            &hole,
            hole_offset,
            &hole_tx.chunk_proof(TxChunkOffset::from(0_u32)).unwrap().0,
        );
        let err = hole
            .generate_full_chunks(hole_offset, hole_offset)
            .expect_err("indexed chunk with no bytes");
        assert!(
            err.to_string()
                .contains("Could not find chunk bytes on disk"),
            "{err}"
        );

        let good = assigned(&config, 1, 1, "hdd1", 4);
        // Zero-fill packing does not match the entropy `unpack` recomputes.
        pack_entropy(&good, &config, 0);
        index_one(&good, &good_tx, 4);
        write_one(&good, &good_tx, &good_data);
        good.sync_pending_chunks().expect("sync");

        let db = create_test_db(&tmp.path().join("db"));
        cache_one(&db, &config, &hole_tx, &hole_data);
        let provider = make_provider(config, vec![Arc::new(hole), Arc::new(good)], db);
        let span = provider
            .get_unpacked_chunks_by_ledger_span(
                DataLedger::Publish,
                LedgerChunkOffset::from(3),
                LedgerChunkOffset::from(4),
            )
            .expect("a bad offset must not fail the span");
        assert_eq!(span.len(), 2);
        assert_eq!(*span[0].0, 3);
        assert_eq!(span[0].1.bytes.0, hole_data);
        assert_eq!(*span[1].0, 4);
        assert_eq!(span[1].1.bytes.0, good_data);

        assert!(
            provider
                .get_chunk_by_ledger_offset(DataLedger::Publish, LedgerChunkOffset::from(100))
                .unwrap()
                .is_none()
        );
        assert!(
            provider
                .get_chunk_by_ledger_offset(DataLedger::Publish, LedgerChunkOffset::from(3))
                .unwrap()
                .is_none(),
            "the ledger route does not pack a cached body"
        );
    }

    /// One cache read error in a span is that offset only. A single-offset
    /// caller still receives the error.
    #[test]
    fn span_omits_one_cache_read_error() -> eyre::Result<()> {
        struct ClearFault;
        impl Drop for ClearFault {
            fn drop(&mut self) {
                set_cache_read_fault(None);
            }
        }
        let _clear = ClearFault;

        let (tmp, config) = opened_span("span-cache-read-error", 4);
        let chunk_size = config.consensus.chunk_size as usize;
        let sm = assigned(&config, 0, 0, "hdd0", 4);
        let bytes = patterned_bytes(0x5A, chunk_size * 3);
        let tx = signed_tx(&config, bytes);
        assert_eq!(tx.data_chunks().unwrap().len(), 3);
        index_placement(&sm, &tx, 0)?;
        let chunks = tx.data_chunks().unwrap();

        let db = create_test_db(&tmp.path().join("db"));
        cache_chunks(&db, &config, &tx, &chunks);
        let provider = make_provider(config, vec![Arc::new(sm)], db);

        let _ = take_chunk_read_notes();
        set_cache_read_fault(Some(1));
        let span = provider
            .get_unpacked_chunks_by_ledger_span(
                DataLedger::Publish,
                LedgerChunkOffset::from(0),
                LedgerChunkOffset::from(2),
            )
            .expect("one cache fault must not fail the span");
        assert_eq!(span.len(), 2, "{span:?}");
        assert_eq!(*span[0].0, 0);
        assert_eq!(span[0].1.bytes.0, chunks[0].bytes.0);
        assert_eq!(*span[1].0, 2);
        assert_eq!(span[1].1.bytes.0, chunks[2].bytes.0);

        let notes = take_chunk_read_notes();
        assert!(
            notes.contains(&("serve_span", "cache", "error")),
            "cache/error was not recorded: {notes:?}"
        );
        assert_eq!(
            notes
                .iter()
                .filter(|note| **note == ("serve_span", "cache", "error"))
                .count(),
            1
        );

        let error = provider
            .get_chunk_by_data_root(
                DataLedger::Publish,
                tx.header.data_tree_root,
                None,
                TxChunkOffset::from(1),
            )
            .expect_err("a single-offset cache read still returns the error");
        assert!(
            error.to_string().contains("injected cache read error"),
            "{error}"
        );
        let kept = provider
            .get_chunk_by_data_root(
                DataLedger::Publish,
                tx.header.data_tree_root,
                None,
                TxChunkOffset::from(0),
            )?
            .expect("the neighboring offset is still served");
        assert_eq!(
            kept.as_unpacked().expect("cache returns unpacked").bytes.0,
            chunks[0].bytes.0
        );

        Ok(())
    }

    /// The tx offset comes from the placement. A stored leaf note of 0, or one
    /// past `u32::MAX` chunks, does not change it and does not drop the offset.
    #[test]
    fn span_takes_the_tx_offset_from_the_placement_not_the_leaf_note() -> eyre::Result<()> {
        let (_tmp, config) = opened_span("span-placement-offset", 8);
        let sm = assigned(&config, 0, 0, "hdd0", 8);
        let (zero_note, bytes_zero) = one_chunk(&config, 1);
        let (wide_note, bytes_wide) = one_chunk(&config, 3);

        pack_entropy(&sm, &config, 0);
        pack_entropy(&sm, &config, 2);
        index_one(&sm, &zero_note, 0);
        index_one(&sm, &wide_note, 2);
        write_one(&sm, &zero_note, &bytes_zero);
        write_one(&sm, &wide_note, &bytes_wide);
        sm.sync_pending_chunks()?;

        let zero_path = leaf_path(0);
        let wide_path = leaf_path(leaf_byte_past_u32_chunks(config.consensus.chunk_size));
        remember_data_path(&sm, PartitionChunkOffset(0), &zero_path);
        remember_data_path(&sm, PartitionChunkOffset(2), &wide_path);

        let packed = sm.generate_full_chunks(PartitionChunkOffset(0), PartitionChunkOffset(2))?;
        let offsets: Vec<u32> = packed.keys().map(|offset| offset.0).collect();
        assert_eq!(offsets, vec![0, 2]);
        for (offset, path) in [(0, &zero_path), (2, &wide_path)] {
            let chunk = &packed[&PartitionChunkOffset(offset)];
            assert_eq!(
                chunk.data_path.0, *path,
                "offset {offset} serves its stored path"
            );
            assert_eq!(
                chunk.tx_offset,
                TxChunkOffset::from(0_u32),
                "offset {offset} is chunk 0 of its one-chunk placement"
            );
        }
        Ok(())
    }

    /// A storage read error is not a hole. A hole stays absent. A span fails
    /// only when every offset in the request failed to read.
    #[test]
    fn storage_read_error_is_not_a_missing_chunk() -> eyre::Result<()> {
        let (tmp, config) = opened_span("chunk-read-error", 4);
        let broken = assigned(&config, 0, 0, "hdd0", 4);
        let (tx, data) = one_chunk(&config, 7);
        pack_entropy(&broken, &config, 0);
        index_one(&broken, &tx, 0);
        write_one(&broken, &tx, &data);
        broken.sync_pending_chunks()?;

        let (hole_tx, _) = one_chunk(&config, 3);
        index_one(&broken, &hole_tx, 1);

        let good = assigned(&config, 1, 1, "hdd1", 4);
        let (good_tx, good_data) = one_chunk(&config, 9);
        pack_entropy(&good, &config, 0);
        index_one(&good, &good_tx, 4);
        write_one(&good, &good_tx, &good_data);
        good.sync_pending_chunks()?;

        let db = create_test_db(&tmp.path().join("db"));
        let provider = make_provider(config.clone(), vec![Arc::new(broken), Arc::new(good)], db);

        assert!(
            provider
                .get_chunk_by_data_root(
                    DataLedger::Publish,
                    tx.header.data_tree_root,
                    None,
                    TxChunkOffset::from(0),
                )?
                .is_some(),
            "the durable chunk is readable before the file is truncated"
        );
        assert!(
            provider
                .get_chunk_by_data_root(
                    DataLedger::Publish,
                    hole_tx.header.data_tree_root,
                    None,
                    TxChunkOffset::from(0),
                )?
                .is_none(),
            "an indexed chunk with no body is a hole"
        );

        let chunks_path = config
            .node_config
            .base_directory
            .join("hdd0")
            .join("chunks.dat");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&chunks_path)?
            .set_len(0)?;

        let err = provider
            .get_chunk_by_data_root(
                DataLedger::Publish,
                tx.header.data_tree_root,
                None,
                TxChunkOffset::from(0),
            )
            .expect_err("a disk read error is not a missing chunk");
        assert!(
            !err.to_string()
                .contains("Could not find chunk bytes on disk"),
            "{err}"
        );

        let span_err = provider
            .get_unpacked_chunks_by_ledger_span(
                DataLedger::Publish,
                LedgerChunkOffset::from(0),
                LedgerChunkOffset::from(0),
            )
            .expect_err("a span whose only offset failed to read");
        assert!(
            !span_err
                .to_string()
                .contains("Could not find chunk bytes on disk"),
            "{span_err}"
        );

        let mixed = provider.get_unpacked_chunks_by_ledger_span(
            DataLedger::Publish,
            LedgerChunkOffset::from(0),
            LedgerChunkOffset::from(4),
        )?;
        assert_eq!(mixed.len(), 1, "{mixed:?}");
        assert_eq!(*mixed[0].0, 4);
        assert_eq!(mixed[0].1.bytes.0, good_data);

        cache_one(&provider.db, &provider.config, &tx, &data);
        let cached = provider
            .get_chunk_by_data_root(
                DataLedger::Publish,
                tx.header.data_tree_root,
                None,
                TxChunkOffset::from(0),
            )?
            .expect("a cache hit is served when the disk read fails");
        assert_eq!(
            cached
                .as_unpacked()
                .expect("cache returns unpacked")
                .bytes
                .0,
            data
        );
        Ok(())
    }

    struct ClearIndexFault;
    impl Drop for ClearIndexFault {
        fn drop(&mut self) {
            set_data_root_index_faults(0);
        }
    }

    /// The first module's index read fails. The second holds the data root, so
    /// the chunk is served. With only the failing module, the error is returned.
    #[test]
    fn placement_scan_serves_a_later_module_after_an_index_error() -> eyre::Result<()> {
        let _clear = ClearIndexFault;
        let (tmp, config) = opened_span("placement-scan-later-module", 4);
        let (tx, data) = one_chunk(&config, 7);
        let broken = assigned(&config, 0, 0, "hdd0", 4);
        let good = assigned(&config, 1, 1, "hdd1", 4);
        pack_entropy(&good, &config, 0);
        index_one(&good, &tx, 4);
        write_one(&good, &tx, &data);
        good.sync_pending_chunks()?;
        let broken = Arc::new(broken);
        let good = Arc::new(good);

        set_data_root_index_faults(1);
        let only = make_provider(
            config.clone(),
            vec![Arc::clone(&broken)],
            create_test_db(&tmp.path().join("empty")),
        );
        let error = only
            .get_chunk_by_data_root(
                DataLedger::Publish,
                tx.header.data_tree_root,
                None,
                TxChunkOffset::from(0),
            )
            .expect_err("an index error with no later module and no cache is returned");
        assert!(
            error.to_string().contains("injected data root index error"),
            "{error}"
        );

        set_data_root_index_faults(1);
        let both = make_provider(
            config.clone(),
            vec![broken, good],
            create_test_db(&tmp.path().join("db")),
        );
        let served = both
            .get_chunk_by_data_root(
                DataLedger::Publish,
                tx.header.data_tree_root,
                None,
                TxChunkOffset::from(0),
            )
            .expect("a later module is tried after an index error")
            .expect("the later module holds the chunk");
        let packed = served
            .as_packed()
            .expect("a storage module returns packed bytes");
        let unpacked = unpack_packed(&config, &packed)?;
        assert_eq!(unpacked.bytes.0, data);
        assert_eq!(unpacked.data_tree_root, tx.header.data_tree_root);
        assert_eq!(*unpacked.tx_offset, 0);
        Ok(())
    }

    /// An index error is not a hole. The cache is still tried, and a hit is served.
    #[test]
    fn placement_scan_serves_the_cache_after_an_index_error() -> eyre::Result<()> {
        let _clear = ClearIndexFault;
        let (tmp, config) = opened_span("placement-scan-cache", 4);
        let (tx, data) = one_chunk(&config, 11);
        let broken = Arc::new(assigned(&config, 0, 0, "hdd0", 4));
        let db = create_test_db(&tmp.path().join("db"));
        cache_one(&db, &config, &tx, &data);
        let provider = make_provider(config, vec![broken], db);

        set_data_root_index_faults(1);
        let served = provider
            .get_chunk_by_data_root(
                DataLedger::Publish,
                tx.header.data_tree_root,
                None,
                TxChunkOffset::from(0),
            )
            .expect("the cache is tried after an index error")
            .expect("the cache holds the chunk");
        assert_eq!(
            served
                .as_unpacked()
                .expect("cache returns unpacked")
                .bytes
                .0,
            data
        );
        Ok(())
    }

    /// The first module's index read fails. A later module still reports the
    /// ledger offsets. With only the failing module, the error is returned.
    #[test]
    fn placement_scan_returns_later_ledger_offsets_after_an_index_error() -> eyre::Result<()> {
        let _clear = ClearIndexFault;
        let (tmp, config) = opened_span("placement-scan-offsets", 4);
        let (tx, _data) = one_chunk(&config, 9);
        let data_commitment = tx.header.data_commitment(&config.data_tree_geometry());
        let broken = Arc::new(assigned(&config, 0, 0, "hdd0", 4));
        let good = assigned(&config, 1, 1, "hdd1", 4);
        index_one(&good, &tx, 4);
        let good = Arc::new(good);

        set_data_root_index_faults(1);
        let only = make_provider(
            config.clone(),
            vec![Arc::clone(&broken)],
            create_test_db(&tmp.path().join("empty")),
        );
        let error = only
            .get_ledger_offsets_for_commitment(DataLedger::Publish, data_commitment)
            .expect_err("an index error with no later module is returned");
        assert!(
            error.to_string().contains("injected data root index error"),
            "{error}"
        );

        set_data_root_index_faults(1);
        let both = make_provider(
            config,
            vec![broken, good],
            create_test_db(&tmp.path().join("db")),
        );
        let offsets = both
            .get_ledger_offsets_for_commitment(DataLedger::Publish, data_commitment)
            .expect("a later module is tried after an index error");
        assert_eq!(offsets, Some(vec![4]));
        Ok(())
    }

    /// A module whose assignment has no ledger slot is skipped. A later module
    /// still answers. A scan of only that module is a miss, not a panic.
    #[test]
    fn placement_scan_skips_a_module_with_no_ledger_slot() -> eyre::Result<()> {
        let (tmp, config) = opened_span("placement-scan-no-slot", 4);
        let (tx, _data) = one_chunk(&config, 3);
        let data_commitment = tx.header.data_commitment(&config.data_tree_geometry());
        let skipped = assigned(&config, 0, 0, "hdd0", 4);
        {
            let mut assignment = skipped
                .partition_assignment
                .read()
                .unwrap()
                .expect("assigned");
            assignment.slot_index = None;
            *skipped.partition_assignment.write().unwrap() = Some(assignment);
        }
        let good = assigned(&config, 1, 1, "hdd1", 4);
        index_one(&good, &tx, 4);
        let skipped = Arc::new(skipped);
        let good = Arc::new(good);

        let alone = make_provider(
            config.clone(),
            vec![Arc::clone(&skipped)],
            create_test_db(&tmp.path().join("empty")),
        );
        let missing =
            alone.get_ledger_offsets_for_commitment(DataLedger::Publish, data_commitment)?;
        assert!(missing.is_none(), "{missing:?}");

        let both = make_provider(
            config,
            vec![skipped, good],
            create_test_db(&tmp.path().join("db")),
        );
        let offsets =
            both.get_ledger_offsets_for_commitment(DataLedger::Publish, data_commitment)?;
        assert_eq!(offsets, Some(vec![4]));
        Ok(())
    }

    fn leaf_path(max_byte: u64) -> Vec<u8> {
        let mut path = vec![0_u8; 64];
        path[56..].copy_from_slice(&max_byte.to_be_bytes());
        path
    }

    /// Smallest leaf note whose chunk index is `u32::MAX + 1`.
    fn leaf_byte_past_u32_chunks(chunk_size: u64) -> u64 {
        let chunks = u64::from(u32::MAX) + 2;
        chunks * chunk_size - chunk_size + 1
    }

    fn write_one(sm: &StorageModule, tx: &irys_types::DataTransaction, bytes: &[u8]) {
        sm.write_data_chunk(&UnpackedChunk {
            data_tree_root: tx.header.data_tree_root,
            data_size: tx.header.data_size,
            data_path: tx.chunk_proof(TxChunkOffset::from(0_u32)).unwrap(),
            bytes: Base64(bytes.to_vec()),
            tx_offset: TxChunkOffset::from(0_u32),
        })
        .expect("write stored chunk");
    }
}
