use eyre::OptionExt as _;
use irys_database::db::IrysDatabaseExt as _;
use irys_database::db_cache::data_size_to_chunk_count;
use irys_database::{cached_chunk_by_chunk_offset, cached_data_root_by_data_root};
use irys_types::{
    ChunkFormat, Config, DataLedger, DataRoot, LedgerChunkOffset, PackedChunk,
    PartitionChunkOffset, RelativeChunkOffset, TxChunkOffset, UnpackedChunk,
    app_state::DatabaseProvider,
};
use tracing::debug;

use crate::{StorageModulesReadGuard, checked_add_i32_u64, get_storage_module_at_offset};

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

    /// Retrieves a chunk from a ledger
    pub fn get_chunk_by_ledger_offset(
        &self,
        ledger: DataLedger,
        ledger_offset: LedgerChunkOffset,
    ) -> eyre::Result<Option<PackedChunk>> {
        // Get basic chunk info
        let module =
            get_storage_module_at_offset(&self.storage_modules_guard, ledger, ledger_offset)
                .ok_or_eyre("No storage module contains this chunk")?;
        module.generate_full_chunk_ledger_offset(ledger_offset)
    }

    /// Retrieves a chunk from a ledger and unpacks it (entropy recompute + XOR + tail trim).
    ///
    /// `Ok(None)` when the offset holds no stored chunk; `Err` when no storage module covers it.
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
            // One read of this root's placements. `generate_full_chunk` reads
            // `DataRootInfos` again to stamp `data_size` from the chunk that
            // actually sits at the partition offset; that value is checked below.
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
    use irys_types::{
        Base64, Config, ConsensusConfig, DataTransactionLedger, H256, LedgerChunkRange, NodeConfig,
        PartitionChunkOffset, UnpackedChunk, irys::IrysSigner, ledger_chunk_offset_ii,
        partition::PartitionAssignment, partition_chunk_offset_ie,
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

    /// Two adjacent placements. `data_tx_offset == chunk_count` of the first tx
    /// is the first byte of the next tx. That offset must not be served.
    #[test]
    fn out_of_range_data_tx_offset_does_not_serve_the_next_placement() -> eyre::Result<()> {
        let tmp_dir = TempDirBuilder::new()
            .prefix("data_root_next_placement")
            .with_tracing()
            .build();
        let config = testing_config(tmp_dir.path().to_path_buf());
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
    #[test]
    fn partial_final_chunk_is_in_range_and_the_next_offset_is_not() -> eyre::Result<()> {
        let tmp_dir = TempDirBuilder::new()
            .prefix("data_root_partial_chunk")
            .with_tracing()
            .build();
        let config = testing_config(tmp_dir.path().to_path_buf());
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
    #[test]
    fn same_data_root_second_placement_does_not_extend_chunk_count() -> eyre::Result<()> {
        let tmp_dir = TempDirBuilder::new()
            .prefix("data_root_two_placements")
            .with_tracing()
            .build();
        let config = testing_config(tmp_dir.path().to_path_buf());
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
        let config = testing_config(tmp_dir.path().to_path_buf());
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

    fn testing_config(base_path: std::path::PathBuf) -> Config {
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: 100,
                ..ConsensusConfig::testing()
            }),
            base_directory: base_path,
            ..NodeConfig::testing()
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
}
