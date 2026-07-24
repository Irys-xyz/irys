use eyre::OptionExt as _;
use irys_database::db::IrysDatabaseExt as _;
use irys_database::{cached_chunk_by_chunk_offset, cached_data_root_by_data_root};
use irys_types::{
    ChunkFormat, Config, DataLedger, DataRoot, LedgerChunkOffset, PackedChunk, TxChunkOffset,
    UnpackedChunk, app_state::DatabaseProvider,
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
}
