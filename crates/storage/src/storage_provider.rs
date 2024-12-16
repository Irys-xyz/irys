use std::{collections::BTreeMap, mem};

use crate::{ChunkType, StorageModules};
use eyre::eyre;
use irys_database::{
    cached_chunk_by_chunk_index, cached_data_root_by_data_root, get_partition_hashes_by_data_root,
    tx_header_by_txid,
};
use irys_types::{
    app_state::DatabaseProvider, ChunkBytes, ChunkEnum, DataRoot, IrysTransactionId,
    LedgerChunkRange, LedgerId, PackedChunk, PartialChunk, PartitionChunkOffset,
    TxRelativeChunkIndex,
};
use nodit::{interval::ii, InclusiveInterval, Interval, NoditMap};
use reth_db::Database;
use tracing::debug;

#[derive(Debug, Clone)]
/// Top-level storage abstraction
pub struct StorageProvider {
    /// Storage modules
    pub storage_modules: StorageModules,
    /// global database env
    pub db: DatabaseProvider,
}

pub type LedgerFullChunkMap = NoditMap<u64, Interval<u64>, Option<ChunkEnum>>;

pub type PartitionFullChunkMap =
    NoditMap<TxRelativeChunkIndex, Interval<TxRelativeChunkIndex>, Option<ChunkEnum>>;

pub type TxRelativeChunkIndexRange = Interval<TxRelativeChunkIndex>;

impl StorageProvider {
    /// Reads a set of chunks by their ledger_id + ledger relative offset range
    // TODO: make this use the Into trait for the offset_range?
    pub fn get_by_ledger_offset(
        &self,
        ledger_id: &LedgerId,
        offset_range: &LedgerChunkRange,
    ) -> eyre::Result<Option<LedgerFullChunkMap>> {
        // map that tracks what intervals we've been able to find, and which we still need to
        let mut request_map: LedgerFullChunkMap = NoditMap::new();
        request_map
            .insert_strict(offset_range.0, None)
            .map_err(|_| eyre!("error building chunk map"))?;

        // try to find these chunks
        // TODO: look through any caches
        // search through storage modules
        for sm in self.storage_modules.iter() {
            if sm.ledger_num() != Some(*ledger_id as u64) {
                continue;
            }

            let read_chunks = match sm.read_full_ledger_chunks(offset_range)? {
                Some(v) => v,
                None => return Ok(None),
            };

            for (interval, full_chunk) in read_chunks {
                debug_assert!(interval.is_singular());
                let ledger_relative = sm.make_range_ledger_relative(interval.into())?;
                // we want to overwrite the existing `None` interval
                let mut cut = request_map.insert_overwrite(*ledger_relative, Some(full_chunk));
                debug_assert!(cut.next().is_some_and(|(_, v)| v.is_none()));
            }
        }

        Ok(Some(request_map))
    }

    /// Reads a set of chunks by their tx_id + tx relative offset range
    pub fn get_by_tx_offset(
        &self,
        tx_id: &IrysTransactionId,
        offset_range: &TxRelativeChunkIndexRange,
    ) -> eyre::Result<Option<PartitionFullChunkMap>> {
        // check confirmed txs
        let tx_header = self.db.view_eyre(|read_tx| {
            tx_header_by_txid(read_tx, &tx_id)?.ok_or(eyre!("Tx is unknown/hasn't been confirmed"))
        })?;
        // get by the data root
        self.get_by_data_root_offset(&tx_header.data_root, offset_range)
    }

    /// Attempts to get all the provided data_root relative range of chunks for a given data_root, first from the global chunk_cache, and then from any storage modules/partitions storing the chunks.
    /// like other storage provider functions, this will return a map containing none values, PackedChunks or UnpackedChunks based on te best-effort of the resolver
    pub fn get_by_data_root_offset(
        &self,
        data_root: &DataRoot,
        offset_range: &TxRelativeChunkIndexRange,
    ) -> eyre::Result<Option<PartitionFullChunkMap>> {
        let mut request_map: PartitionFullChunkMap = NoditMap::new();
        request_map
            .insert_strict(*offset_range, None)
            .map_err(|_| eyre!("error building chunk map"))?;

        // check global cache
        let read_tx = self.db.tx()?;
        let cached_data_root = cached_data_root_by_data_root(&read_tx, data_root)?;

        for offset in offset_range.start()..=offset_range.end() {
            let mut partial_chunk = PartialChunk {
                data_root: Some(*data_root),
                chunk_index: Some(offset),
                ..Default::default()
            };
            // TODO: make a PartialChunkEnum so we make sure we don't mix packed and unpacked data
            // we don't check cache unless there's a cached data root
            match cached_data_root {
                Some(ref cached_data_root) => {
                    partial_chunk.data_size = Some(cached_data_root.data_size);

                    // TOOD: bounds check the offset range based on the cached_data_root's data_size
                    // now we read as many chunks as we can from CachedChunksIndex
                    let chunk = cached_chunk_by_chunk_index(&read_tx, data_root, &offset)?;
                    match chunk {
                        Some((_index_meta, cached_chunk)) => {
                            match cached_chunk.chunk {
                                Some(cached_chunk_data) => {
                                    partial_chunk.bytes = Some(cached_chunk_data)
                                }
                                None => (),
                            };
                            partial_chunk.data_path = Some(cached_chunk.data_path)
                        }
                        None => continue,
                    }
                }
                None => (),
            };
            if partial_chunk.is_full_unpacked_chunk() {
                // chunk data can only be unpacked here
                debug!(
                    "got full chunk index {} for data root {} from cache",
                    &offset, &data_root
                );
                let mut cut = request_map.insert_overwrite(
                    ii(offset, offset),
                    Some(ChunkEnum::Unpacked(partial_chunk.try_into()?)),
                );
                debug_assert!(cut.next().is_some_and(|(_, v)| v.is_none()));
                continue;
            }
        }

        // now we need to retrieve chunks we didn't get from the cache
        // TODO: improve this so we only get fields we weren't able to get from cache, instead of discarding partial chunks

        // get all the `None` intervals
        let missing_chunks = request_map
            .iter()
            .filter(|(_, chunk)| chunk.is_none())
            .collect::<Vec<_>>();
        // if we have no missing, return
        if missing_chunks.len() == 0 {
            return Ok(Some(request_map));
        }
        // TODO: optimise this so that we can request the same single top level range but not actually read any data for already gotten chunks to reduce overhead
        // maybe pass the request_map in to each stage/read function directly? or some sort of mapped version (interval -> true/false) that lets them know what reads they can skip

        // now we read from storage_modules
        // all SM's *should* have the same state for the same data_root/chunks, but it's possible the one we pick is a.) syncing or b.) a partial store (tx is on a partition boundary)
        // we don't handle case a for now, but should.

        let storing_partitions = match get_partition_hashes_by_data_root(&read_tx, data_root)? {
            Some(hashes) => hashes,
            None => {
                debug!("unable to find any local partitions storing {}", data_root);
                return Ok(Some(request_map));
            }
        };

        for sm in self.storage_modules.iter() {
            // skip this SM if it doesn't have an assignment, or
            match sm.partition_assignment {
                Some(partition_assignment) => {
                    if !storing_partitions
                        .0
                        .contains(&partition_assignment.partition_hash)
                    {
                        continue;
                    }
                }
                None => continue,
            };

            let start_offsets = sm.collect_start_offsets(data_root)?;

            for start_offset in start_offsets.0 {
                let missing_chunks = request_map
                    .iter()
                    // clone due to mutable-immutable borrowing
                    .filter_map(|(interval, chunk)| chunk.is_none().then(|| interval.clone()))
                    .collect::<Vec<_>>();

                for missing in missing_chunks {
                    let missing_translated = match translate_and_clamp(missing, start_offset) {
                        Some(v) => v,
                        None => continue,
                    };
                    // try reading the missing range from this sm & start offset
                    let mut read_chunks = match sm.read_full_ledger_chunks(
                        &ii(
                            missing_translated.start() as u64,
                            missing_translated.end() as u64,
                        )
                        .into(),
                    )? {
                        Some(v) => v,
                        None => continue, // skip this range
                    };

                    for (interval, chunk) in read_chunks.iter_mut() {
                        assert!(interval.is_singular());
                        let mut cut = request_map.insert_overwrite(
                            translate_back(*interval, start_offset),
                            // TODO: improve this (wrap ChunkEnum in Option?)
                            Some(mem::replace(
                                chunk,
                                ChunkEnum::Packed(PackedChunk::default()),
                            )),
                        );
                        debug_assert!(cut.next().is_some_and(|(_, v)| v.is_none()));
                    }
                }
            }
        }

        Ok(Some(request_map))
    }
}

/// Translates interval by offset and clamps negative values to 0
fn translate_and_clamp(interval: Interval<u32>, offset: i32) -> Option<Interval<u32>> {
    let translated_start = interval.start() as i32 + offset;
    let translated_end = interval.end() as i32 + offset;
    if translated_start <= 0 && translated_end <= 0 {
        return None;
    }
    Some(ii(
        translated_start.max(0) as u32,
        translated_end.max(0) as u32,
    ))
}

/// Translates interval back by applying the inverse of the offset
fn translate_back(interval: Interval<u32>, offset: i32) -> Interval<u32> {
    let offset_abs = offset.unsigned_abs();
    if offset < 0 {
        ii(interval.start() + offset_abs, interval.end() + offset_abs)
    } else {
        ii(interval.start() - offset_abs, interval.end() - offset_abs)
    }
}

#[cfg(test)]
mod translation_tests {
    use super::*;

    #[test]
    fn test_negative_offset() {
        let interval = ii(5, 12);
        let offset = -10;

        let clamped = translate_and_clamp(interval, offset).unwrap();
        assert_eq!(clamped, ii(0, 2), "After clamping");

        let final_result = translate_back(clamped, offset);
        assert_eq!(final_result, ii(10, 12), "After translation back");
    }

    #[test]
    fn test_positive_offset() {
        let interval = ii(5, 12);
        let offset = 3;

        let clamped = translate_and_clamp(interval, offset).unwrap();
        assert_eq!(clamped, ii(8, 15), "After clamping");

        let final_result = translate_back(clamped, offset);
        assert_eq!(final_result, ii(5, 12), "After translation back");
    }

    #[test]
    fn test_complete_negative_interval() {
        let interval = ii(5, 12);
        let offset = -20;
        let clamped = translate_and_clamp(interval, offset);
        assert_eq!(clamped, None, "After clamping");
    }

    #[test]
    fn test_zero_offset() {
        let interval = ii(5, 12);
        let offset = 0;

        let clamped = translate_and_clamp(interval, offset).unwrap();
        assert_eq!(clamped, ii(5, 12), "After clamping");

        let final_result = translate_back(clamped, offset);
        assert_eq!(final_result, ii(5, 12), "After translation back");
    }
}

#[cfg(test)]
mod storage_provider_tests {
    use std::sync::Arc;

    use irys_database::{assign_data_root, open_or_create_db, tables::IrysTables};
    use irys_testing_utils::utils::setup_tracing_and_temp_dir;
    use irys_types::{
        app_state::DatabaseProvider, irys::IrysSigner, partition::PartitionAssignment, Base64,
        LedgerChunkRange, StorageConfig, UnpackedChunk, H256,
    };
    use nodit::{interval::ii, InclusiveInterval};
    use rand::Rng as _;
    use reth_db::Database;
    use tracing::debug;

    use crate::{
        initialize_storage_files, StorageModule, StorageModuleInfo, StorageModules, StorageProvider,
    };

    #[test]
    fn test_storage_provider_ledger_range() -> eyre::Result<()> {
        let storage_module_infos = vec![
            StorageModuleInfo {
                id: 0,
                partition_assignment: Some(PartitionAssignment {
                    partition_hash: Default::default(),
                    miner_address: Default::default(),
                    ledger_num: Some(1),
                    slot_index: Some(0),
                }),
                submodules: vec![
                    (ii(0, 4), "hdd0-4TB".to_string()), // 0 to 4 inclusive
                    (ii(5, 9), "hdd1-4TB".to_string()), // 5 to 9 inclusive
                ],
            },
            StorageModuleInfo {
                id: 1,
                partition_assignment: Some(PartitionAssignment {
                    partition_hash: Default::default(),
                    miner_address: Default::default(),
                    ledger_num: Some(1),
                    slot_index: Some(1),
                }),
                submodules: vec![
                    (ii(0, 4), "hdd2-4TB".to_string()), // 0 to 4 inclusive
                    (ii(5, 9), "hdd3-4TB".to_string()), // 5 to 9 inclusive
                ],
            },
        ];

        // Override the default StorageModule config for testing
        let storage_config = StorageConfig {
            min_writes_before_sync: 0,
            chunk_size: 32,
            num_chunks_in_partition: 10,
            ..Default::default()
        };

        let tmp_dir = setup_tracing_and_temp_dir(Some("test_storage_provider_ledger_range"), false);
        let base_path = tmp_dir.path().to_path_buf();
        let db = open_or_create_db(&base_path, IrysTables::ALL, None).unwrap();
        let arc_db = Arc::new(db);
        let db_provider = DatabaseProvider(arc_db.clone());

        // TODO: add validation that storage modules are actually `num_chunks_in_partition`
        // probably should add this to initialize_storage_files, as it's a common
        let _ = initialize_storage_files(&base_path, &storage_module_infos);
        let mut storage_modules: Vec<Arc<StorageModule>> = Vec::new();
        for info in storage_module_infos {
            let arc_module = Arc::new(StorageModule::new(
                &base_path,
                &info,
                Some(storage_config.clone()),
            ));
            storage_modules.push(arc_module.clone());
            arc_module.pack_with_zeros();
        }

        // generate legitimate transaction

        // Create 12.5 chunks worth of data *  fill the data with random bytes
        let data_size = (storage_config.chunk_size as f64 * 12.5).round() as usize;
        let mut data_bytes = vec![0u8; data_size];
        rand::thread_rng().fill(&mut data_bytes[..]);

        // Create a new Irys signed transaction
        let irys = IrysSigner::random_signer_with_chunk_size(storage_config.chunk_size);
        let tx = irys.create_transaction(data_bytes.clone(), None).unwrap();
        let tx = irys.sign_transaction(tx).unwrap();

        let data_root = tx.header.data_root;
        let data_size = tx.header.data_size;

        let chunk_range: LedgerChunkRange = ii(0, tx.chunks.len() as u64).into();
        // equivalent to update_storage_module_indexes
        for storage_module in &storage_modules {
            storage_module.index_transaction_data(vec![0], data_root, chunk_range)?
        }

        for (index, chunk_node) in tx.chunks.iter().enumerate() {
            let min = chunk_node.min_byte_range;
            let max = chunk_node.max_byte_range;
            let data_path = Base64(tx.proofs[index].proof.to_vec());
            // skip chunk 4 & 5 (sm1 submodule boundary)
            if index == 4 || index == 5 {
                continue;
            }

            let chunk = UnpackedChunk {
                data_root,
                data_size,
                data_path,
                bytes: Base64(data_bytes[min..max].to_vec()),
                chunk_index: index as u32,
            };
            // storage_module.write_data_chunk(&chunk)?;
            for sm in &storage_modules {
                if sm.get_write_offsets(&chunk).unwrap_or(vec![]).len() != 0 {
                    sm.write_data_chunk(&chunk)?;
                }
            }
        }

        for sm in &storage_modules {
            sm.sync_pending_chunks()?;
        }

        let storage_provider = StorageProvider {
            storage_modules: StorageModules(storage_modules),
            db: db_provider,
        };

        let res = storage_provider
            .get_by_ledger_offset(&1, &ii(chunk_range.start(), chunk_range.end() + 1).into())?
            .unwrap();

        let results_vec = res
            .iter()
            .map(|(i, c)| (*i, c.is_some()))
            .collect::<Vec<_>>();

        assert_eq!(
            results_vec,
            vec![
                (ii(0, 0), true),
                (ii(1, 1), true),
                (ii(2, 2), true),
                (ii(3, 3), true),
                (ii(4, 5), false),
                (ii(6, 6), true),
                (ii(7, 7), true),
                (ii(8, 8), true),
                (ii(9, 9), true),
                (ii(10, 10), true),
                (ii(11, 11), true),
                (ii(12, 12), true),
                (ii(13, 14), false)
            ]
        );

        Ok(())
    }

    #[test]
    fn test_storage_provider_cache() -> eyre::Result<()> {
        // Override the default StorageModule config for testing
        let storage_config = StorageConfig {
            min_writes_before_sync: 0,
            chunk_size: 32,
            num_chunks_in_partition: 10,
            ..Default::default()
        };

        let tmp_dir = setup_tracing_and_temp_dir(Some("test_storage_provider_cache"), false);
        let base_path = tmp_dir.path().to_path_buf();
        let db = Arc::new(open_or_create_db(&base_path, IrysTables::ALL, None).unwrap());
        let db_provider = DatabaseProvider(db.clone());

        // generate legitimate transaction

        // Create 12.5 chunks worth of data *  fill the data with random bytes
        let data_size = (storage_config.chunk_size as f64 * 12.5).round() as usize;
        let mut data_bytes = vec![0u8; data_size];
        rand::thread_rng().fill(&mut data_bytes[..]);

        // Create a new Irys signed transaction
        let irys = IrysSigner::random_signer_with_chunk_size(storage_config.chunk_size);
        let tx = irys.create_transaction(data_bytes.clone(), None).unwrap();
        let tx = irys.sign_transaction(tx).unwrap();

        let data_root = tx.header.data_root;
        let data_size = tx.header.data_size;

        // cache the data root
        db.update_eyre(|db_tx| irys_database::cache_data_root(db_tx, &tx.header))?;

        // cache each chunk
        for (index, chunk_node) in tx.chunks.iter().enumerate() {
            let min = chunk_node.min_byte_range;
            let max = chunk_node.max_byte_range;
            let data_path = Base64(tx.proofs[index].proof.to_vec());

            // // skip chunk 4
            if index == 4 {
                continue;
            }

            let chunk = UnpackedChunk {
                data_root,
                data_size,
                data_path,
                bytes: Base64(data_bytes[min..max].to_vec()),
                chunk_index: index as u32,
            };

            db.update_eyre(|tx| irys_database::cache_chunk(tx, &chunk))?;
        }

        // create storage provider

        let storage_provider = StorageProvider {
            storage_modules: StorageModules(vec![]),
            db: db_provider,
        };

        // get by data_root + relative offset
        let res = storage_provider
            .get_by_data_root_offset(&data_root, &ii(0, 14))?
            .unwrap();

        debug!("got chunks {:?}", &res);

        let results_vec = res
            .iter()
            .map(|(i, c)| (*i, c.is_some()))
            .collect::<Vec<_>>();

        debug!("results {:?}", &results_vec);

        assert_eq!(
            results_vec,
            vec![
                (ii(0, 0), true),
                (ii(1, 1), true),
                (ii(2, 2), true),
                (ii(3, 3), true),
                (ii(4, 4), false),
                (ii(5, 5), true),
                (ii(6, 6), true),
                (ii(7, 7), true),
                (ii(8, 8), true),
                (ii(9, 9), true),
                (ii(10, 10), true),
                (ii(11, 11), true),
                (ii(12, 12), true),
                (ii(13, 14), false)
            ]
        );

        Ok(())
    }

    #[test]
    fn test_storage_provider_mix() -> eyre::Result<()> {
        let storage_module_infos = vec![
            StorageModuleInfo {
                id: 0,
                partition_assignment: Some(PartitionAssignment {
                    partition_hash: H256::random(),
                    miner_address: Default::default(),
                    ledger_num: Some(1),
                    slot_index: Some(0),
                }),
                submodules: vec![
                    (ii(0, 4), "hdd0-4TB".to_string()), // 0 to 4 inclusive
                    (ii(5, 9), "hdd1-4TB".to_string()), // 5 to 9 inclusive
                ],
            },
            StorageModuleInfo {
                id: 1,
                partition_assignment: Some(PartitionAssignment {
                    partition_hash: H256::random(),
                    miner_address: Default::default(),
                    ledger_num: Some(1),
                    slot_index: Some(1),
                }),
                submodules: vec![
                    (ii(0, 4), "hdd2-4TB".to_string()), // 0 to 4 inclusive
                    (ii(5, 9), "hdd3-4TB".to_string()), // 5 to 9 inclusive
                ],
            },
        ];

        // Override the default StorageModule config for testing
        let storage_config = StorageConfig {
            min_writes_before_sync: 0,
            chunk_size: 32,
            num_chunks_in_partition: 10,
            ..Default::default()
        };

        let tmp_dir = setup_tracing_and_temp_dir(Some("test_storage_provider_mix"), false);
        let base_path = tmp_dir.path().to_path_buf();
        let db = open_or_create_db(&base_path, IrysTables::ALL, None).unwrap();
        let arc_db = Arc::new(db);
        let db_provider = DatabaseProvider(arc_db.clone());

        // TODO: add validation that storage modules are actually `num_chunks_in_partition`
        // probably should add this to initialize_storage_files, as it's a common
        let _ = initialize_storage_files(&base_path, &storage_module_infos);
        let mut storage_modules: Vec<Arc<StorageModule>> = Vec::new();
        for info in storage_module_infos {
            let arc_module = Arc::new(StorageModule::new(
                &base_path,
                &info,
                Some(storage_config.clone()),
            ));
            storage_modules.push(arc_module.clone());
            arc_module.pack_with_zeros();
        }

        // generate legitimate transaction

        // Create 12.5 chunks worth of data *  fill the data with random bytes
        let data_size = (storage_config.chunk_size as f64 * 12.5).round() as usize;
        let mut data_bytes = vec![0u8; data_size];
        rand::thread_rng().fill(&mut data_bytes[..]);

        // Create a new Irys signed transaction
        let irys = IrysSigner::random_signer_with_chunk_size(storage_config.chunk_size);
        let tx = irys.create_transaction(data_bytes.clone(), None).unwrap();
        let tx = irys.sign_transaction(tx).unwrap();

        let data_root = tx.header.data_root;
        let data_size = tx.header.data_size;

        let chunk_range: LedgerChunkRange = ii(0, tx.chunks.len() as u64).into();
        // equivalent to update_storage_module_indexes
        for storage_module in &storage_modules {
            arc_db.update_eyre(|tx| {
                assign_data_root(tx, data_root, storage_module.partition_hash().unwrap())
            })?;
            storage_module.index_transaction_data(vec![0], data_root, chunk_range)?
        }
        // cache the data root
        arc_db.update_eyre(|db_tx| irys_database::cache_data_root(db_tx, &tx.header))?;

        for (index, chunk_node) in tx.chunks.iter().enumerate() {
            let min = chunk_node.min_byte_range;
            let max = chunk_node.max_byte_range;
            let data_path = Base64(tx.proofs[index].proof.to_vec());
            // skip chunk 4 & 5, add to the cache

            let chunk = UnpackedChunk {
                data_root,
                data_size,
                data_path,
                bytes: Base64(data_bytes[min..max].to_vec()),
                chunk_index: index as u32,
            };

            if index == 4 || index == 5 {
                arc_db.update_eyre(|tx| irys_database::cache_chunk(tx, &chunk))?;
                continue;
            }
            // storage_module.write_data_chunk(&chunk)?;
            for sm in &storage_modules {
                if sm.get_write_offsets(&chunk).unwrap_or(vec![]).len() != 0 {
                    sm.write_data_chunk(&chunk)?;
                }
            }
        }

        for sm in &storage_modules {
            sm.sync_pending_chunks()?;
        }

        let storage_provider = StorageProvider {
            storage_modules: StorageModules(storage_modules),
            db: db_provider,
        };

        // get by data_root + relative offset
        let res = storage_provider
            .get_by_data_root_offset(&data_root, &ii(0, 14))?
            .unwrap();

        let results_vec = res
            .iter()
            .map(|(i, c)| (*i, c.is_some()))
            .collect::<Vec<_>>();

        assert_eq!(
            results_vec,
            vec![
                (ii(0, 0), true),
                (ii(1, 1), true),
                (ii(2, 2), true),
                (ii(3, 3), true),
                (ii(4, 5), false),
                (ii(6, 6), true),
                (ii(7, 7), true),
                (ii(8, 8), true),
                (ii(9, 9), true),
                (ii(10, 14), false)
            ]
        );

        Ok(())
    }
}
