use irys_database::db::IrysDatabaseExt as _;
use irys_database::db_cache::data_size_to_chunk_count;
use irys_database::{cached_chunk_by_chunk_offset, cached_data_root_by_data_root};
use irys_types::{
    ChunkFormat, Config, DataLedger, DataRoot, LedgerChunkOffset, LedgerChunkRange, PackedChunk,
    PartitionChunkOffset, RelativeChunkOffset, TxChunkOffset, UnpackedChunk,
    app_state::DatabaseProvider,
};
use nodit::interval::ii;
use opentelemetry::{KeyValue, global, metrics::Counter};
use std::sync::{
    LazyLock,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};
use tracing::{debug, warn};

use crate::models::index_read_metrics::{self, SERVE_DATA_ROOT, SERVE_LEDGER, SERVE_SPAN};
use crate::{
    StorageModulesReadGuard, checked_add_i32_u64, get_overlapped_storage_modules,
    get_storage_module_at_offset,
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
}

#[cfg(test)]
fn set_cache_read_fault(tx_offset: Option<u32>) {
    CACHE_READ_FAULT.with(|cell| cell.set(tx_offset));
}

#[cfg(test)]
fn take_chunk_read_notes() -> Vec<(&'static str, &'static str, &'static str)> {
    CHUNK_READ_NOTES.with(|notes| std::mem::take(&mut *notes.borrow_mut()))
}

fn missing_on_disk(error: &eyre::Report) -> bool {
    error
        .to_string()
        .contains("Could not find chunk bytes on disk")
}

fn unpack_packed(config: &Config, packed: &PackedChunk) -> UnpackedChunk {
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

    /// Retrieves a chunk from a ledger and unpacks it (entropy recompute + XOR + tail trim).
    ///
    /// `Ok(None)` when no body is stored. This does not read the chunk cache.
    pub fn get_unpacked_chunk_by_ledger_offset(
        &self,
        ledger: DataLedger,
        ledger_offset: LedgerChunkOffset,
    ) -> eyre::Result<Option<UnpackedChunk>> {
        let Some(packed) = self.get_chunk_by_ledger_offset(ledger, ledger_offset)? else {
            return Ok(None);
        };
        let consensus = &self.config.consensus;
        Ok(Some(irys_packing::unpack(
            &packed,
            consensus.entropy_packing_iterations,
            consensus.chunk_size as usize,
            consensus.chain_id,
        )))
    }

    /// Unpacked chunks for the inclusive ledger span `[from, to]`.
    ///
    /// Each storage module that overlaps the span is read once. Offsets the
    /// node does not hold are absent. The result is ordered by ledger offset.
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
        let requested = LedgerChunkRange(ii(from, to));
        let modules =
            get_overlapped_storage_modules(&self.storage_modules_guard, ledger, &requested);
        let mut out = Vec::new();
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
                let packed = if let Some(map) = batch.as_mut() {
                    map.remove(&partition_offset)
                } else {
                    match module.generate_full_chunk(partition_offset) {
                        Ok(chunk) => chunk,
                        Err(error) => {
                            debug!(
                                error = %error,
                                partition.offset = cursor,
                                "skipping chunk offset after a storage module read error"
                            );
                            note_chunk_read(SERVE_SPAN, "storage_module", "error");
                            None
                        }
                    }
                };
                if let Some(packed) = packed {
                    note_chunk_read(SERVE_SPAN, "storage_module", "hit");
                    out.push((ledger_offset, unpack_packed(&self.config, &packed)));
                } else {
                    // The batch path omitted a hole. The error path already counted it.
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
        let Some((data_root, tx_offset)) = placement else {
            return Ok(None);
        };
        self.cached_unpacked(caller, data_root, tx_offset)
    }

    fn cached_unpacked(
        &self,
        caller: &'static str,
        data_root: DataRoot,
        tx_offset: TxChunkOffset,
    ) -> eyre::Result<Option<UnpackedChunk>> {
        match self.get_chunk_from_cache(data_root, tx_offset) {
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

    /// Retrieves a chunk by [`DataRoot`] + tx-relative offset.
    ///
    /// Lookup order:
    /// 1. Local storage modules assigned to `ledger` (returns packed when present).
    /// 2. Node chunk cache (`CachedChunks`) — allows non-assignees (upload targets /
    ///    ingress-proof generators) to serve bodies that never landed in an SM.
    ///
    /// Cache hits are returned as [`ChunkFormat::Unpacked`]. Callers that need
    /// packed form (or that accept either) should handle both variants — e.g.
    /// block validation already does.
    ///
    /// Note: ingress-proof gossip is not chunk replication. Cache-backed serve is
    /// best-effort; pruned cache entries yield `Ok(None)` the same as a missing SM.
    ///
    /// `Ok(None)` also when `data_tx_offset` is outside `[0, chunk_count)` for the
    /// placement (`data_size` on the storage-module index, or on `CachedDataRoots`).
    /// A chunk read from that offset is returned only when its `data_root`,
    /// `data_size`, and `tx_offset` are the requested placement.
    pub fn get_chunk_by_data_root(
        &self,
        ledger: DataLedger,
        data_root: DataRoot,
        data_tx_offset: TxChunkOffset,
    ) -> eyre::Result<Option<ChunkFormat>> {
        index_read_metrics::with_caller(SERVE_DATA_ROOT, || {
            self.data_root_in_caller(ledger, data_root, data_tx_offset)
        })
    }

    fn data_root_in_caller(
        &self,
        ledger: DataLedger,
        data_root: DataRoot,
        data_tx_offset: TxChunkOffset,
    ) -> eyre::Result<Option<ChunkFormat>> {
        debug!(
            "getting ledger: {:?}, data_root: {}, offset: {}",
            &ledger, &data_root, &data_tx_offset
        );
        let chunk_size = self.config.consensus.chunk_size;
        let num_chunks_in_partition = self.config.consensus.num_chunks_in_partition;
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

        for sm in sms {
            // One read of this root's placements. `generate_full_chunk` stamps
            // `data_root`, `data_size` and `tx_offset` from the interval that holds
            // the partition offset; those values are checked below.
            let data_root_infos = sm.collect_data_root_infos(data_root)?;
            for info in &data_root_infos.0 {
                if !tx_offset_within_data_size(data_tx_offset, info.data_size, chunk_size) {
                    continue;
                }
                let Some(partition_offset) = partition_offset_for_placement(
                    info.start_offset,
                    data_tx_offset,
                    num_chunks_in_partition,
                ) else {
                    continue;
                };
                // try other offsets and sm's if we get an Error or a None
                let Ok(Some(packed)) = sm.generate_full_chunk(partition_offset) else {
                    continue;
                };
                if packed.data_root != data_root
                    || packed.tx_offset != data_tx_offset
                    || packed.data_size != info.data_size
                {
                    debug!(
                        requested_data_root = %data_root,
                        requested_tx_offset = %data_tx_offset,
                        chunk_data_root = %packed.data_root,
                        chunk_tx_offset = %packed.tx_offset,
                        chunk_data_size = packed.data_size,
                        placement_data_size = info.data_size,
                        %partition_offset,
                        "data_root chunk does not match the requested placement"
                    );
                    continue;
                }
                return Ok(Some(ChunkFormat::Packed(packed)));
            }
        }
        drop(binding);

        // Fall back to the node chunk cache. Non-assignees often never have an SM
        // for this ledger/slot, but may still hold the body after POST /chunk
        // (long enough to generate an ingress proof and to serve residual holes).
        self.get_chunk_from_cache(data_root, data_tx_offset)
    }

    /// Reads an unpacked chunk body from `CachedChunks` / `CachedDataRoots`.
    ///
    /// Returns `Ok(None)` when the data_root metadata is missing, the offset is
    /// outside `[0, chunk_count(data_size))`, the offset is not indexed, or the
    /// body was pruned (`chunk: None`).
    ///
    /// A cache row stores bytes and `data_path` only. `data_root`, `data_size`,
    /// and `tx_offset` on the response are the index key and `CachedDataRoots`.
    /// Those are the fields the chunk-count check accepts.
    fn get_chunk_from_cache(
        &self,
        data_root: DataRoot,
        data_tx_offset: TxChunkOffset,
    ) -> eyre::Result<Option<ChunkFormat>> {
        #[cfg(test)]
        if CACHE_READ_FAULT.with(std::cell::Cell::get) == Some(*data_tx_offset) {
            return Err(eyre::eyre!("injected cache read error"));
        }
        let chunk_size = self.config.consensus.chunk_size;
        self.db.view_eyre(|tx| {
            let Some(cdr) = cached_data_root_by_data_root(tx, data_root)? else {
                return Ok(None);
            };
            if !tx_offset_within_data_size(data_tx_offset, cdr.data_size, chunk_size) {
                debug!(
                    data_root = %data_root,
                    tx_offset = %data_tx_offset,
                    data_size = cdr.data_size,
                    "data_root offset is past the cached data size"
                );
                return Ok(None);
            }
            let Some((_meta, cached)) =
                cached_chunk_by_chunk_offset(tx, data_root, data_tx_offset)?
            else {
                return Ok(None);
            };
            // Index-only rows (body pruned / stored only in a partition) cannot be served.
            let Some(bytes) = cached.chunk else {
                debug!(
                    data_root = %data_root,
                    tx_offset = %data_tx_offset,
                    "cache index hit but chunk body missing; cannot serve data_root fetch"
                );
                return Ok(None);
            };

            Ok(Some(ChunkFormat::Unpacked(UnpackedChunk {
                data_root,
                data_size: cdr.data_size,
                data_path: cached.data_path,
                bytes,
                tx_offset: data_tx_offset,
            })))
        })
    }

    pub fn get_ledger_offsets_for_data_root(
        &self,
        ledger: DataLedger,
        data_root: DataRoot,
    ) -> eyre::Result<Option<Vec<u64>>> {
        debug!("getting ledger: {:?}, data_root: {}", &ledger, &data_root,);

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

        // TODO: see if we should check the DataRootInfo.data_size here too

        // find a SM that contains this data root, return the start_offsets once we find it
        for sm in sms {
            let sm_range_start = sm.get_storage_module_ledger_offsets().unwrap().start();
            let data_root_infos = sm.collect_data_root_infos(data_root)?;
            let mapped_offsets = data_root_infos
                .0
                .iter()
                .filter_map(|info| {
                    checked_add_i32_u64(*info.start_offset, sm_range_start.into())
                    // translate into ledger-relative space
                })
                .collect::<Vec<_>>();

            if !mapped_offsets.is_empty() {
                return Ok(Some(mapped_offsets));
            }
        }
        Ok(None)
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
        Base64, Config, ConsensusConfig, DataTransaction, DataTransactionLedger, H256,
        LedgerChunkOffset, LedgerChunkRange, NodeConfig, PartitionChunkOffset, UnpackedChunk,
        irys::IrysSigner, ledger_chunk_offset_ii, partition::PartitionAssignment,
        partition_chunk_offset_ie,
    };
    use nodit::interval::ii;
    use rand::Rng as _;
    use rstest::rstest;

    #[test]
    fn get_by_data_tx_offset_test() -> eyre::Result<()> {
        let tmp_dir = TempDirBuilder::new()
            .prefix("get_by_data_tx_offset_test")
            .with_tracing()
            .build();
        let base_path = tmp_dir.path().to_path_buf();
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: 100,
                ..ConsensusConfig::testing()
            }),
            base_directory: base_path,
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);
        let infos = [StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment::default()),
            submodules: vec![
                (partition_chunk_offset_ie!(0, 50), "hdd0".into()),
                (partition_chunk_offset_ie!(50, 100), "hdd1".into()),
            ],
        }];

        // Create a StorageModule with the specified submodules and config
        let storage_module_info = &infos[0];
        let storage_module = StorageModule::new(storage_module_info, &config)?;

        let data_size = (config.consensus.chunk_size as f64 * 2.5).round() as usize;
        let mut data_bytes = vec![0_u8; data_size];
        rand::thread_rng().fill(&mut data_bytes[..]);

        let irys = IrysSigner::random_signer(&config.consensus);
        let tx = irys
            .create_transaction(data_bytes.clone(), H256::zero())
            .unwrap();
        let tx = irys.sign_transaction(tx).unwrap();

        // fake the tx_path
        // Create a tx_root (and paths) from the tx
        let (_tx_root, proofs) =
            DataTransactionLedger::merklize_tx_root(std::slice::from_ref(&tx.header));

        let tx_path = &proofs[0].proof;

        // let data_root = H256::zero();
        let data_root = tx.header.data_root;
        // Pack the storage module

        storage_module.pack_with_zeros();

        let chunk_range = ledger_chunk_offset_ii!(49, 51);
        let _ = storage_module.index_transaction_data(
            &tx.header,
            tx_path,
            LedgerChunkRange(chunk_range),
            0,
        );

        let mut unpacked_chunks = vec![];
        for (tx_chunk_offset, chunk_node) in tx.chunks.iter().enumerate() {
            let min = chunk_node.min_byte_range;
            let max = chunk_node.max_byte_range;
            // let offset = tx.proofs[tx_chunk_offset].offset as u32;
            let data_path = Base64(tx.proofs[tx_chunk_offset].proof.clone());
            // let key: H256 = hash_sha256(&data_path.0).unwrap().into();
            let chunk_bytes = Base64(data_bytes[min..max].to_vec());
            let chunk = UnpackedChunk {
                data_root,
                data_size: data_size as u64,
                data_path: data_path.clone(),
                bytes: chunk_bytes.clone(),
                tx_offset: TxChunkOffset::from(
                    TryInto::<u32>::try_into(tx_chunk_offset).expect("Value exceeds u32::MAX"),
                ),
            };
            storage_module.write_data_chunk(&chunk)?;
            unpacked_chunks.push(chunk);
        }
        storage_module.sync_pending_chunks()?;

        let storage_modules_guard =
            StorageModulesReadGuard::new(Arc::new(RwLock::new(vec![Arc::new(storage_module)])));

        // Empty main DB is fine: SM path still serves packed chunks.
        let db_path = tmp_dir.path().join("irys_db");
        let db = create_test_db(&db_path);
        let chunk_provider = ChunkProvider::new(config.clone(), storage_modules_guard, db);

        for original_chunk in unpacked_chunks {
            let chunk = chunk_provider
                .get_chunk_by_data_root(DataLedger::Publish, data_root, original_chunk.tx_offset)?
                .unwrap();
            let packed_chunk = chunk.as_packed().unwrap();

            let unpacked_data = unpack_with_entropy(
                &packed_chunk,
                vec![0_u8; config.consensus.chunk_size as usize],
                config.consensus.chunk_size as usize,
            );
            let unpacked_chunk = UnpackedChunk {
                data_root: packed_chunk.data_root,
                data_size: packed_chunk.data_size,
                data_path: packed_chunk.data_path.clone(),
                bytes: Base64(unpacked_data),
                tx_offset: packed_chunk.tx_offset,
            };
            assert_eq!(original_chunk, unpacked_chunk);
        }

        Ok(())
    }

    /// Non-assignee / no-SM case: body only in the node chunk cache must still
    /// be served via data_root + tx_offset (enables residual-hole heal from
    /// ingress-proof generators that never held a ledger assignment).
    #[test]
    fn get_by_data_root_serves_from_chunk_cache_without_sm() -> eyre::Result<()> {
        let tmp_dir = TempDirBuilder::new()
            .prefix("get_by_data_root_cache_test")
            .with_tracing()
            .build();
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: 100,
                ..ConsensusConfig::testing()
            }),
            base_directory: tmp_dir.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);

        let data_size = (config.consensus.chunk_size as f64 * 2.5).round() as usize;
        let mut data_bytes = vec![0_u8; data_size];
        rand::thread_rng().fill(&mut data_bytes[..]);

        let irys = IrysSigner::random_signer(&config.consensus);
        let tx = irys
            .create_transaction(data_bytes.clone(), H256::zero())
            .unwrap();
        let tx = irys.sign_transaction(tx).unwrap();
        let data_root = tx.header.data_root;

        let db = create_test_db(&tmp_dir.path().join("irys_db"));
        db.update_eyre(|wtx| {
            cache_data_root(wtx, &tx.header, None)?;
            for (tx_chunk_offset, chunk_node) in tx.chunks.iter().enumerate() {
                let min = chunk_node.min_byte_range;
                let max = chunk_node.max_byte_range;
                let chunk = UnpackedChunk {
                    data_root,
                    data_size: data_size as u64,
                    data_path: Base64(tx.proofs[tx_chunk_offset].proof.clone()),
                    bytes: Base64(data_bytes[min..max].to_vec()),
                    tx_offset: TxChunkOffset::from(
                        TryInto::<u32>::try_into(tx_chunk_offset).expect("Value exceeds u32::MAX"),
                    ),
                };
                cache_chunk(wtx, &chunk)?;
            }
            Ok(())
        })?;

        // No storage modules: simulates an upload/API node that is not assigned
        // to the Submit slot but still holds cache after POST /chunk.
        let storage_modules_guard = StorageModulesReadGuard::new(Arc::new(RwLock::new(Vec::new())));
        let chunk_provider = ChunkProvider::new(config, storage_modules_guard, db);

        for (tx_chunk_offset, chunk_node) in tx.chunks.iter().enumerate() {
            let offset = TxChunkOffset::from(
                TryInto::<u32>::try_into(tx_chunk_offset).expect("Value exceeds u32::MAX"),
            );
            let min = chunk_node.min_byte_range;
            let max = chunk_node.max_byte_range;
            let expected = UnpackedChunk {
                data_root,
                data_size: data_size as u64,
                data_path: Base64(tx.proofs[tx_chunk_offset].proof.clone()),
                bytes: Base64(data_bytes[min..max].to_vec()),
                tx_offset: offset,
            };

            let served = chunk_provider
                .get_chunk_by_data_root(DataLedger::Submit, data_root, offset)?
                .expect("cache-backed data_root fetch should find body");
            let unpacked = served
                .as_unpacked()
                .expect("cache path returns ChunkFormat::Unpacked");
            assert_eq!(unpacked, expected);
        }

        // Missing offset → None (not an error)
        let missing = TxChunkOffset::from(99_u32);
        assert!(
            chunk_provider
                .get_chunk_by_data_root(DataLedger::Submit, data_root, missing)?
                .is_none()
        );

        Ok(())
    }

    /// Two adjacent placements. `data_tx_offset == chunk_count` of the first tx
    /// is the first byte of the next tx. That offset must not be served.
    #[rstest]
    #[case::mdbx(SubmoduleIndexEngine::Mdbx)]
    #[case::rocksdb(SubmoduleIndexEngine::Rocksdb)]
    fn out_of_range_data_tx_offset_does_not_serve_the_next_placement(
        #[case] engine: SubmoduleIndexEngine,
    ) -> eyre::Result<()> {
        let tmp_dir = TempDirBuilder::new()
            .prefix("data_root_next_placement")
            .with_tracing()
            .build();
        let config = bounds_testing_config(tmp_dir.path().to_path_buf(), engine);
        let storage_module = testing_publish_module(&config)?;
        let chunk_size = config.consensus.chunk_size as usize;

        let bytes_a = patterned_bytes(0xA1, chunk_size * 2);
        let bytes_b = patterned_bytes(0xB2, chunk_size * 2);
        let tx_a = signed_tx(&config, bytes_a.clone());
        let tx_b = signed_tx(&config, bytes_b.clone());
        assert_ne!(tx_a.header.data_root, tx_b.header.data_root);
        assert_eq!(tx_a.chunks.len(), 2);
        assert_eq!(tx_a.header.data_size, bytes_a.len() as u64);

        // First tx occupies ledger offsets 10..=11. The next tx starts at 12,
        // which is `start_offset + chunk_count` for the first tx.
        index_placement(&storage_module, &tx_a, 10)?;
        index_placement(&storage_module, &tx_b, 12)?;
        let chunks_a = unpacked_chunks_for(&tx_a, &bytes_a);
        let chunks_b = unpacked_chunks_for(&tx_b, &bytes_b);
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
        assert_offset_absent(&provider, tx_a.header.data_root, 2)?;

        Ok(())
    }

    /// `data_size` is not a multiple of `chunk_size`. The short final chunk is
    /// in range. The next offset is the following placement.
    #[rstest]
    #[case::mdbx(SubmoduleIndexEngine::Mdbx)]
    #[case::rocksdb(SubmoduleIndexEngine::Rocksdb)]
    fn partial_final_chunk_is_in_range_and_the_next_offset_is_not(
        #[case] engine: SubmoduleIndexEngine,
    ) -> eyre::Result<()> {
        let tmp_dir = TempDirBuilder::new()
            .prefix("data_root_partial_chunk")
            .with_tracing()
            .build();
        let config = bounds_testing_config(tmp_dir.path().to_path_buf(), engine);
        let storage_module = testing_publish_module(&config)?;
        let chunk_size = config.consensus.chunk_size as usize;
        let short_len = chunk_size / 2;
        let bytes_a = patterned_bytes(0x11, chunk_size * 2 + short_len);
        let bytes_b = patterned_bytes(0x22, chunk_size);
        let tx_a = signed_tx(&config, bytes_a.clone());
        let tx_b = signed_tx(&config, bytes_b.clone());
        assert_eq!(tx_a.chunks.len(), 3);
        assert_eq!(tx_a.header.data_size, bytes_a.len() as u64);

        // Offsets 20..=22 are the three chunks of A (the last is short).
        // Offset 23 is B, which `start + chunk_count` names.
        index_placement(&storage_module, &tx_a, 20)?;
        index_placement(&storage_module, &tx_b, 23)?;
        let chunks_a = unpacked_chunks_for(&tx_a, &bytes_a);
        let chunks_b = unpacked_chunks_for(&tx_b, &bytes_b);
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
        assert_offset_absent(&provider, tx_a.header.data_root, 3)?;

        Ok(())
    }

    /// The same data root stored twice, back to back. An offset past
    /// `chunk_count` names the second placement's first chunk and must not
    /// be returned as if it were that offset.
    #[rstest]
    #[case::mdbx(SubmoduleIndexEngine::Mdbx)]
    #[case::rocksdb(SubmoduleIndexEngine::Rocksdb)]
    fn same_data_root_second_placement_does_not_extend_chunk_count(
        #[case] engine: SubmoduleIndexEngine,
    ) -> eyre::Result<()> {
        let tmp_dir = TempDirBuilder::new()
            .prefix("data_root_two_placements")
            .with_tracing()
            .build();
        let config = bounds_testing_config(tmp_dir.path().to_path_buf(), engine);
        let storage_module = testing_publish_module(&config)?;
        let chunk_size = config.consensus.chunk_size as usize;

        let bytes = patterned_bytes(0x42, chunk_size * 2);
        let tx = signed_tx(&config, bytes.clone());
        assert_eq!(tx.chunks.len(), 2);

        // Placements at 30..=31 and 32..=33. Offset 2 of the first placement
        // is partition offset 32, the second placement's chunk 0.
        index_placement(&storage_module, &tx, 30)?;
        index_placement(&storage_module, &tx, 32)?;
        let chunks = unpacked_chunks_for(&tx, &bytes);
        write_chunks(&storage_module, &chunks)?;
        storage_module.sync_pending_chunks()?;

        let data_root = tx.header.data_root;
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
        assert_eq!(second_placement.data_root, data_root);
        assert_eq!(second_placement.tx_offset, TxChunkOffset::from(0_u32));
        assert_offset_absent(&provider, data_root, 2)?;

        Ok(())
    }

    /// A cached body stored past `chunk_count(data_size)` is not served.
    /// The cache key is `(data_root, tx_offset)`, so this does not alias into
    /// another tx; the bound is the data size on `CachedDataRoots`.
    #[test]
    fn cache_does_not_serve_offset_past_data_size() -> eyre::Result<()> {
        let tmp_dir = TempDirBuilder::new()
            .prefix("data_root_cache_bounds")
            .with_tracing()
            .build();
        let config =
            bounds_testing_config(tmp_dir.path().to_path_buf(), SubmoduleIndexEngine::Mdbx);
        let chunk_size = config.consensus.chunk_size as usize;
        let bytes = patterned_bytes(0x7C, chunk_size * 2);
        let tx = signed_tx(&config, bytes.clone());
        assert_eq!(tx.chunks.len(), 2);
        let data_root = tx.header.data_root;
        let chunks = unpacked_chunks_for(&tx, &bytes);
        let past_end = TxChunkOffset::from(2_u32);

        let db = create_test_db(&tmp_dir.path().join("irys_db"));
        db.update_eyre(|wtx| {
            cache_data_root(wtx, &tx.header, None)?;
            for chunk in &chunks {
                cache_chunk(wtx, chunk)?;
            }
            // Distinct data_path so the cache does not treat this as a duplicate
            // of a real chunk and skip the out-of-range index entry.
            cache_chunk(
                wtx,
                &UnpackedChunk {
                    data_root,
                    data_size: bytes.len() as u64,
                    data_path: Base64(vec![0xEE, 0xEE, 0xEE, past_end.0 as u8]),
                    bytes: Base64(vec![0xFF; 4]),
                    tx_offset: past_end,
                },
            )?;
            Ok(())
        })?;

        let storage_modules_guard = StorageModulesReadGuard::new(Arc::new(RwLock::new(Vec::new())));
        let chunk_provider = ChunkProvider::new(config, storage_modules_guard, db);

        let last = chunk_provider
            .get_chunk_by_data_root(DataLedger::Submit, data_root, chunks[1].tx_offset)?
            .ok_or_else(|| eyre::eyre!("last valid cached chunk missing"))?;
        assert_eq!(
            last.as_unpacked().expect("cache returns unpacked"),
            chunks[1]
        );

        let served =
            chunk_provider.get_chunk_by_data_root(DataLedger::Submit, data_root, past_end)?;
        if let Some(chunk) = served {
            eyre::bail!("cached offset {past_end} past data_size was served: {chunk:?}");
        }

        Ok(())
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
        let chunk_count = tx.chunks.len() as u64;
        let range = LedgerChunkRange(ledger_chunk_offset_ii!(
            ledger_start,
            ledger_start + chunk_count - 1
        ));
        storage_module.index_transaction_data(&tx.header, &proofs[0].proof, range, 0)
    }

    fn unpacked_chunks_for(tx: &irys_types::DataTransaction, bytes: &[u8]) -> Vec<UnpackedChunk> {
        let data_root = tx.header.data_root;
        let data_size = bytes.len() as u64;
        tx.chunks
            .iter()
            .enumerate()
            .map(|(tx_chunk_offset, chunk_node)| UnpackedChunk {
                data_root,
                data_size,
                data_path: Base64(tx.proofs[tx_chunk_offset].proof.clone()),
                bytes: Base64(bytes[chunk_node.min_byte_range..chunk_node.max_byte_range].to_vec()),
                tx_offset: TxChunkOffset::from(
                    u32::try_from(tx_chunk_offset).expect("chunk offset exceeds u32"),
                ),
            })
            .collect()
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
        let storage_modules_guard =
            StorageModulesReadGuard::new(Arc::new(RwLock::new(vec![Arc::new(storage_module)])));
        ChunkProvider::new(config, storage_modules_guard, create_test_db(db_path))
    }

    fn assert_serves_packed(
        provider: &ChunkProvider,
        config: &Config,
        expected: &UnpackedChunk,
    ) -> eyre::Result<()> {
        let Some(chunk) = provider.get_chunk_by_data_root(
            DataLedger::Publish,
            expected.data_root,
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
        );
        let unpacked = UnpackedChunk {
            data_root: packed.data_root,
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
        data_root: DataRoot,
        offset: u32,
    ) -> eyre::Result<()> {
        let served = provider.get_chunk_by_data_root(
            DataLedger::Publish,
            data_root,
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

    fn cache_one(db: &DatabaseProvider, tx: &DataTransaction, data: &[u8]) {
        db.update_eyre(|wtx| {
            cache_data_root(wtx, &tx.header, None)?;
            cache_chunk(
                wtx,
                &UnpackedChunk {
                    data_root: tx.header.data_root,
                    data_size: tx.header.data_size,
                    data_path: Base64(tx.proofs[0].proof.clone()),
                    bytes: Base64(data.to_vec()),
                    tx_offset: TxChunkOffset::from(0_u32),
                },
            )?;
            Ok(())
        })
        .unwrap();
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
        let tmp = TempDirBuilder::new()
            .prefix("ledger-no-sm")
            .with_tracing()
            .build();
        let config = testing_config(tmp.path().to_path_buf(), 4);
        let db = create_test_db(&tmp.path().join("db"));
        let provider = make_provider(config, Vec::new(), db);

        let chunk = provider
            .get_chunk_by_ledger_offset(DataLedger::Publish, LedgerChunkOffset::from(0))
            .unwrap();
        assert!(chunk.is_none());
    }

    #[test]
    fn span_uses_the_cache_when_the_module_has_no_body() {
        let tmp = TempDirBuilder::new()
            .prefix("ledger-cache-fallback")
            .with_tracing()
            .build();
        let config = testing_config(tmp.path().to_path_buf(), 4);
        let sm = assigned(&config, 0, 0, "hdd0", 4);
        let (tx, data) = one_chunk(&config, 7);
        index_one(&sm, &tx, 0);
        let sm = Arc::new(sm);

        let db = create_test_db(&tmp.path().join("db"));
        cache_one(&db, &tx, &data);
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

    #[test]
    fn span_skips_a_module_error_and_keeps_the_other_chunk() {
        let tmp = TempDirBuilder::new()
            .prefix("span-skip-module-error")
            .with_tracing()
            .build();
        let config = testing_config(tmp.path().to_path_buf(), 4);
        let (hole_tx, hole_data) = one_chunk(&config, 3);
        let (good_tx, good_data) = one_chunk(&config, 9);

        let hole = assigned(&config, 0, 0, "hdd0", 4);
        index_one(&hole, &hole_tx, 3);
        let hole_offset = PartitionChunkOffset(3);
        remember_data_path(&hole, hole_offset, &hole_tx.proofs[0].proof);
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
        good.write_data_chunk(&UnpackedChunk {
            data_root: good_tx.header.data_root,
            data_size: good_tx.header.data_size,
            data_path: Base64(good_tx.proofs[0].proof.clone()),
            bytes: Base64(good_data.clone()),
            tx_offset: TxChunkOffset::from(0_u32),
        })
        .expect("write stored chunk");
        good.sync_pending_chunks().expect("sync");

        let db = create_test_db(&tmp.path().join("db"));
        cache_one(&db, &hole_tx, &hole_data);
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

        let tmp = TempDirBuilder::new()
            .prefix("span-cache-read-error")
            .with_tracing()
            .build();
        let config = testing_config(tmp.path().to_path_buf(), 4);
        let chunk_size = config.consensus.chunk_size as usize;
        let sm = assigned(&config, 0, 0, "hdd0", 4);
        let bytes = patterned_bytes(0x5A, chunk_size * 3);
        let tx = signed_tx(&config, bytes.clone());
        assert_eq!(tx.chunks.len(), 3);
        index_placement(&sm, &tx, 0)?;
        let chunks = unpacked_chunks_for(&tx, &bytes);

        let db = create_test_db(&tmp.path().join("db"));
        db.update_eyre(|wtx| {
            cache_data_root(wtx, &tx.header, None)?;
            for chunk in &chunks {
                cache_chunk(wtx, chunk)?;
            }
            Ok(())
        })?;
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
                tx.header.data_root,
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
                tx.header.data_root,
                TxChunkOffset::from(0),
            )?
            .expect("the neighboring offset is still served");
        assert_eq!(
            kept.as_unpacked().expect("cache returns unpacked").bytes.0,
            chunks[0].bytes.0
        );

        Ok(())
    }

    /// A stored leaf note of 0 underflows the chunk index. A note past
    /// `u32::MAX` chunks does not fit `TxChunkOffset`. Neither panics, and a
    /// span still returns the other offsets.
    #[test]
    fn span_omits_a_data_path_with_an_unusable_leaf_offset() -> eyre::Result<()> {
        let tmp = TempDirBuilder::new()
            .prefix("span-bad-leaf")
            .with_tracing()
            .build();
        let config = testing_config(tmp.path().to_path_buf(), 8);
        let sm = assigned(&config, 0, 0, "hdd0", 8);
        let (good_a, bytes_a) = one_chunk(&config, 1);
        let (bad_zero, _) = one_chunk(&config, 2);
        let (good_b, bytes_b) = one_chunk(&config, 3);
        let (bad_wide, _) = one_chunk(&config, 4);

        pack_entropy(&sm, &config, 0);
        pack_entropy(&sm, &config, 2);
        index_one(&sm, &good_a, 0);
        index_one(&sm, &bad_zero, 1);
        index_one(&sm, &good_b, 2);
        index_one(&sm, &bad_wide, 3);
        write_one(&sm, &good_a, &bytes_a);
        write_one(&sm, &good_b, &bytes_b);
        sm.sync_pending_chunks()?;

        remember_data_path(&sm, PartitionChunkOffset(1), &leaf_path(0));
        let wide = leaf_byte_past_u32_chunks(config.consensus.chunk_size);
        remember_data_path(&sm, PartitionChunkOffset(3), &leaf_path(wide));

        let before = crate::models::index_read_metrics::bad_leaf_offset_for_test();
        let packed = sm.generate_full_chunks(PartitionChunkOffset(0), PartitionChunkOffset(3))?;
        let offsets: Vec<u32> = packed.keys().map(|offset| offset.0).collect();
        assert_eq!(offsets, vec![0, 2]);
        assert_eq!(
            crate::models::index_read_metrics::bad_leaf_offset_for_test() - before,
            2
        );
        assert!(sm.generate_full_chunk(PartitionChunkOffset(1))?.is_none());
        assert!(sm.generate_full_chunk(PartitionChunkOffset(3))?.is_none());

        let db = create_test_db(&tmp.path().join("db"));
        let provider = make_provider(config, vec![Arc::new(sm)], db);
        let span = provider
            .get_unpacked_chunks_by_ledger_span(
                DataLedger::Publish,
                LedgerChunkOffset::from(0),
                LedgerChunkOffset::from(3),
            )
            .expect("a bad leaf must not fail the span");
        assert_eq!(span.len(), 2, "{span:?}");
        assert_eq!(*span[0].0, 0);
        assert_eq!(span[0].1.bytes.0, bytes_a);
        assert_eq!(*span[1].0, 2);
        assert_eq!(span[1].1.bytes.0, bytes_b);
        assert!(
            provider
                .get_chunk_by_ledger_offset(DataLedger::Publish, LedgerChunkOffset::from(1))?
                .is_none()
        );

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
            data_root: tx.header.data_root,
            data_size: tx.header.data_size,
            data_path: Base64(tx.proofs[0].proof.clone()),
            bytes: Base64(bytes.to_vec()),
            tx_offset: TxChunkOffset::from(0_u32),
        })
        .expect("write stored chunk");
    }
}
