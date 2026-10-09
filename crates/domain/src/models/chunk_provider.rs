use irys_database::db::IrysDatabaseExt as _;
use irys_database::{cached_chunk_by_chunk_offset, cached_data_root_by_data_root};
use irys_types::{
    ChunkFormat, Config, DataLedger, DataRoot, LedgerChunkOffset, LedgerChunkRange, PackedChunk,
    PartitionChunkOffset, TxChunkOffset, UnpackedChunk, app_state::DatabaseProvider,
};
use nodit::interval::ii;
use opentelemetry::{KeyValue, global, metrics::Counter};
use std::sync::LazyLock;
use tracing::debug;

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
                    if let Some(cached) =
                        self.cached_unpacked_at(SERVE_SPAN, &module, partition_offset)?
                    {
                        out.push((ledger_offset, cached));
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

        // TODO: this also needs to be checked for data_size constraints
        // So you can't get a chunk that overlaps the next txs chunks / start_offset

        for sm in sms {
            let data_root_infos1 = sm.collect_data_root_infos(data_root)?;
            let offsets = data_root_infos1
                .0
                .iter()
                .map(|info| info.start_offset + (*data_tx_offset as i32))
                .collect::<Vec<_>>();

            for part_relative_offset in offsets {
                // try other offsets and sm's if we get an Error or a None
                if let Ok(Some(r)) = sm.generate_full_chunk(part_relative_offset.into()) {
                    return Ok(Some(ChunkFormat::Packed(r)));
                }
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
    /// not indexed, or the body was pruned (`chunk: None`).
    fn get_chunk_from_cache(
        &self,
        data_root: DataRoot,
        data_tx_offset: TxChunkOffset,
    ) -> eyre::Result<Option<ChunkFormat>> {
        self.db.view_eyre(|tx| {
            let Some(cdr) = cached_data_root_by_data_root(tx, data_root)? else {
                return Ok(None);
            };
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use crate::{StorageModule, StorageModuleInfo};

    use super::*;
    use irys_database::{cache_chunk, cache_data_root};
    use irys_packing::unpack_with_entropy;
    use irys_testing_utils::utils::TempDirBuilder;
    use irys_types::{
        Base64, Config, ConsensusConfig, DataTransaction, DataTransactionLedger, H256,
        LedgerChunkOffset, LedgerChunkRange, NodeConfig, PartitionChunkOffset, UnpackedChunk,
        irys::IrysSigner, ledger_chunk_offset_ii, partition::PartitionAssignment,
        partition_chunk_offset_ie,
    };
    use nodit::interval::ii;
    use rand::Rng as _;

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
}
