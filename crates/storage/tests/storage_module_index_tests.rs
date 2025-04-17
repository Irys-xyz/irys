use std::sync::Arc;

use irys_database::{
    cache_chunk, cached_chunk_by_chunk_offset, open_or_create_db,
    submodule::{
        get_data_size_by_data_root, get_full_tx_path, get_path_hashes_by_offset,
        get_start_offsets_by_data_root,
    },
    tables::IrysTables,
};
use irys_storage::*;
use irys_testing_utils::utils::setup_tracing_and_temp_dir;
use irys_types::{
    irys::IrysSigner, ledger_chunk_offset_ii, partition::PartitionAssignment,
    partition_chunk_offset_ie, partition_chunk_offset_ii, Base64, CombinedConfig, ConsensusConfig,
    ConsensusOptions, DataTransactionLedger, IrysTransaction, IrysTransactionHeader,
    LedgerChunkOffset, LedgerChunkRange, NodeConfig, PartitionChunkOffset, PartitionChunkRange,
    TxChunkOffset, UnpackedChunk, H256,
};
use openssl::sha;
use reth_db::Database;
use tracing::info;

#[test]
fn tx_path_overlap_tests() -> eyre::Result<()> {
    let mut node_config = NodeConfig::testnet();
    node_config.storage.num_writes_before_sync = 1;
    node_config.consensus = ConsensusOptions::Custom(ConsensusConfig {
        chunk_size: 32,
        num_chunks_in_partition: 20,
        chunk_migration_depth: 1,
        num_chunks_in_recall_range: 5,
        num_partitions_per_slot: 1,
        entropy_packing_iterations: 1,
        ..node_config.consensus_config()
    });
    let config = CombinedConfig::new(node_config);

    // Configure 3 storage modules that are assigned to the submit ledger in
    // slots 0, 1, and 2
    let storage_module_infos = vec![
        StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment {
                partition_hash: H256::random(),
                miner_address: config.node_config.miner_address(),
                ledger_id: Some(1),
                slot_index: Some(0), // Submit Ledger Slot 0
            }),
            submodules: vec![
                (partition_chunk_offset_ii!(0, 4), "hdd0".into()), // 0 to 4 inclusive
                (partition_chunk_offset_ii!(5, 9), "hdd1".into()), // 5 to 9 inclusive
                (partition_chunk_offset_ii!(10, 19), "hdd2".into()), // 10 to 19 inclusive
            ],
        },
        StorageModuleInfo {
            id: 1,
            partition_assignment: Some(PartitionAssignment {
                partition_hash: H256::random(),
                miner_address: config.node_config.miner_address(),
                ledger_id: Some(1),
                slot_index: Some(1), // Submit Ledger Slot 1
            }),
            submodules: vec![
                (partition_chunk_offset_ii!(0, 9), "hdd3".into()), // 0 to 9 inclusive
                (
                    ii(
                        PartitionChunkOffset::from(10),
                        PartitionChunkOffset::from(19),
                    ),
                    "hdd4".into(),
                ), // 10 to 19 inclusive
            ],
        },
        StorageModuleInfo {
            id: 2,
            partition_assignment: Some(PartitionAssignment {
                partition_hash: H256::random(),
                miner_address: config.node_config.miner_address(),
                ledger_id: Some(1),
                slot_index: Some(2), // Submit Ledger Slot 2
            }),
            submodules: vec![
                (
                    ii(
                        PartitionChunkOffset::from(0),
                        PartitionChunkOffset::from(19),
                    ),
                    "hdd5".into(),
                ), // 0 to 19 inclusive
            ],
        },
    ];

    let tmp_dir = setup_tracing_and_temp_dir(Some("storage_module_test"), false);
    let base_path = tmp_dir.path().to_path_buf();
    info!("temp_dir:{:?}\nbase_path:{:?}", tmp_dir, base_path);

    let mut storage_modules: Vec<Arc<StorageModule>> = Vec::new();

    // Create a Vec initialized storage modules
    for info in storage_module_infos {
        let arc_module = Arc::new(StorageModule::new(&info, &config)?);
        storage_modules.push(arc_module.clone());
        arc_module.pack_with_zeros();
    }

    let partition_0_range = LedgerChunkRange(ledger_chunk_offset_ii!(0, 19));
    let partition_1_range = LedgerChunkRange(ledger_chunk_offset_ii!(20, 39));

    // Create a list of BLOBs that represent transaction data
    let data_chunks = vec![
        vec![[0; 32], [1; 32], [2; 32]], // Fill most of one submodule
        vec![[3; 32], [4; 32], [5; 32]], // Overlap the next submodule
        vec![
            [6; 32], [7; 32], [8; 32], [9; 32], [10; 32], [11; 32], [12; 32], [13; 32], [14; 32],
            [15; 32], [16; 32], [17; 32], [18; 32],
        ], // Stop one short of filling the StorageModule
        vec![[19; 32], [20; 32], [21; 32]], // Overlap the next StorageModule
        vec![[22; 32], [23; 32], [24; 32], [25; 32], [26; 32], [27; 32]], // Perfectly fills the submodule without overlapping
    ];

    // Helpful logging when debugging
    // let mut chunk_offset = 0;
    // for chunk_group in &data_chunks {
    //     for chunk in chunk_group {
    //         println!("write[{:?}]: {:?}", chunk_offset, chunk);
    //         chunk_offset += 1;
    //     }
    // }

    // Loop though all the data_chunks and create wrapper tx for them
    let signer = config.irys_signer();
    let mut txs: Vec<IrysTransaction> = Vec::new();

    for chunks in data_chunks {
        let mut data: Vec<u8> = Vec::new();
        for chunk in chunks {
            data.extend_from_slice(&chunk);
        }
        let tx = signer.create_transaction(data, None).unwrap();
        let tx = signer.sign_transaction(tx).unwrap();
        txs.push(tx);
    }

    let tx_headers: Vec<IrysTransactionHeader> = txs.iter().map(|tx| tx.header.clone()).collect();

    // Create a tx_root (and paths) from the tx
    let (_tx_root, proofs) = DataTransactionLedger::merklize_tx_root(&tx_headers);

    // Assume this is the first block in the blockchain
    let proof = &proofs[0];
    let tx_path = &proof.proof;

    // Tx:1 - Base case, write tx index data without any overlaps
    let num_chunks_in_tx = (proof.offset + 1) as u64 / config.consensus.chunk_size;
    let (tx_ledger_range, tx_partition_range) = calculate_tx_ranges(
        LedgerChunkOffset::from(0),
        &partition_0_range,
        proof.offset as u64,
        config.consensus.chunk_size,
    );

    let data_root = tx_headers[0].data_root;
    let data_size = tx_headers[0].data_size;
    let _ = storage_modules[0].index_transaction_data(
        tx_path.clone(),
        data_root,
        tx_ledger_range,
        data_size,
    );

    // Get the submodule reference
    let submodule = storage_modules[0]
        .get_submodule(PartitionChunkOffset::from(0))
        .ok_or(eyre::eyre!("Storage module not found"))
        .unwrap();

    // Verify the tx_path_hash and tx_path bytes were added
    let tx_path_hash = H256::from(hash_sha256(tx_path).unwrap());
    verify_tx_path_in_submodule(submodule, tx_path, tx_path_hash);

    verify_tx_path_offsets(submodule, tx_path_hash, tx_partition_range, &[]);

    verify_data_root_start_offset(submodule, data_root, 0);

    verify_data_root_data_size(submodule, data_root, data_size);

    // Tx:2 - Overlapping case, tx chunks start in one submodule and go to another
    let start_chunk_offset = LedgerChunkOffset::from(num_chunks_in_tx);
    let bytes_in_tx = proofs[1].offset as u64 - proof.offset as u64;
    let (tx_ledger_range, tx_partition_range) = calculate_tx_ranges(
        start_chunk_offset,
        &partition_0_range,
        bytes_in_tx,
        config.consensus.chunk_size,
    );
    let tx_path = &proofs[1].proof;
    let data_root = tx_headers[1].data_root;
    let data_size = tx_headers[1].data_size;
    assert_eq!(data_size, bytes_in_tx);
    let _ = storage_modules[0].index_transaction_data(
        tx_path.clone(),
        data_root,
        tx_ledger_range,
        data_size,
    );

    // Get the both submodule references
    let submodule = storage_modules[0]
        .get_submodule(PartitionChunkOffset::from(0))
        .ok_or(eyre::eyre!("Storage module not found"))
        .unwrap();

    let submodule2 = storage_modules[0]
        .get_submodule(tx_partition_range.end())
        .ok_or(eyre::eyre!("Storage module not found"))
        .unwrap();

    // Verify the tx_path_hash and tx_path bytes were added to both submodules
    let tx_path_hash = H256::from(hash_sha256(tx_path).unwrap());

    verify_tx_path_in_submodule(submodule, tx_path, tx_path_hash);
    verify_tx_path_in_submodule(submodule2, tx_path, tx_path_hash);

    verify_tx_path_offsets(submodule, tx_path_hash, tx_partition_range, &[5]);
    verify_tx_path_offsets(submodule2, tx_path_hash, tx_partition_range, &[3, 4]);

    verify_data_root_start_offset(submodule, data_root, 3);
    verify_data_root_start_offset(submodule2, data_root, 3);

    verify_data_root_data_size(submodule, data_root, data_size);
    verify_data_root_data_size(submodule2, data_root, data_size);

    // Tx:3 - Fill up the StorageModule leaving one empty chunk
    let tx_path = &proofs[2].proof;
    let data_root = tx_headers[2].data_root;
    let offset = proofs[2].offset as u64;
    let bytes_in_tx =
        (offset + 1) - (*(tx_ledger_range.end() + 1u64) * config.consensus.chunk_size);
    let data_size = tx_headers[2].data_size;
    assert_eq!(bytes_in_tx, data_size);
    let start_chunk_offset = tx_ledger_range.end() + 1u64;
    let (tx_ledger_range, tx_partition_range) = calculate_tx_ranges(
        start_chunk_offset,
        &partition_0_range,
        bytes_in_tx,
        config.consensus.chunk_size,
    );
    let _ = storage_modules[0].index_transaction_data(
        tx_path.clone(),
        data_root,
        tx_ledger_range,
        data_size,
    );

    let submodule3 = storage_modules[0]
        .get_submodule(tx_partition_range.end())
        .ok_or(eyre::eyre!("Storage module not found"))
        .unwrap();

    // Verify the tx_path hash and bytes were added to the 2nd submodule
    let tx_path_hash = H256::from(hash_sha256(tx_path).unwrap());
    verify_tx_path_in_submodule(submodule2, tx_path, tx_path_hash);
    verify_tx_path_in_submodule(submodule3, tx_path, tx_path_hash);

    verify_tx_path_offsets(
        submodule2,
        tx_path_hash,
        tx_partition_range,
        &[10, 11, 12, 13, 14, 15, 16, 17, 18],
    );
    verify_tx_path_offsets(
        submodule3,
        tx_path_hash,
        tx_partition_range,
        &[6, 7, 8, 9, 10],
    );

    verify_data_root_start_offset(submodule2, data_root, 6);
    verify_data_root_start_offset(submodule3, data_root, 6);

    verify_data_root_data_size(submodule2, data_root, data_size);
    verify_data_root_data_size(submodule3, data_root, data_size);

    // Tx:4 - Overlap between StorageModules
    let tx_path = &proofs[3].proof;
    let data_root = tx_headers[3].data_root;
    let data_size = tx_headers[3].data_size;
    let offset = proofs[3].offset as u64;
    let bytes_in_tx =
        (offset + 1) - (*(tx_ledger_range.end() + 1u64) * config.consensus.chunk_size);
    assert_eq!(bytes_in_tx, data_size);
    let start_chunk_offset = tx_ledger_range.end() + 1u64;
    let (tx_ledger_range, tx_partition_range) = calculate_tx_ranges(
        start_chunk_offset,
        &partition_0_range,
        bytes_in_tx,
        config.consensus.chunk_size,
    );
    // Update both storage modules with the tx data
    let _ = storage_modules[0].index_transaction_data(
        tx_path.clone(),
        data_root,
        tx_ledger_range,
        data_size,
    );
    let _ = storage_modules[1].index_transaction_data(
        tx_path.clone(),
        data_root,
        tx_ledger_range,
        data_size,
    );

    // The first submodule of the second StorageModule/Partition
    let submodule4 = storage_modules[1]
        .get_submodule(PartitionChunkOffset::from(0))
        .ok_or(eyre::eyre!("Storage module not found"))
        .unwrap();

    // Verify the tx_path hash and bytes were added to the 2nd submodule
    let tx_path_hash = H256::from(hash_sha256(tx_path).unwrap());
    verify_tx_path_in_submodule(submodule3, tx_path, tx_path_hash);
    verify_tx_path_in_submodule(submodule4, tx_path, tx_path_hash);

    verify_tx_path_offsets(submodule3, tx_path_hash, tx_partition_range, &[20, 21]);

    // We now need ranges relative to the second partition
    let (_tx_ledger_range, tx_partition_range) = calculate_tx_ranges(
        start_chunk_offset,
        &partition_1_range,
        bytes_in_tx,
        config.consensus.chunk_size,
    );

    verify_tx_path_offsets(submodule4, tx_path_hash, tx_partition_range, &[]);

    verify_data_root_start_offset(submodule3, data_root, 19);
    verify_data_root_start_offset(submodule4, data_root, -1); // Offset is from previous Partition

    verify_data_root_data_size(submodule3, data_root, data_size);
    verify_data_root_data_size(submodule4, data_root, data_size);

    // Tx:5 - Perfectly fills the submodule without overlapping
    let tx_path = &proofs[4].proof;
    let data_root = tx_headers[4].data_root;
    let data_size = tx_headers[4].data_size;
    let offset = proofs[4].offset as u64;
    let bytes_in_tx = (offset + 1) - ((*tx_ledger_range.end() + 1) * config.consensus.chunk_size);
    assert_eq!(bytes_in_tx, data_size);
    let start_chunk_offset = tx_ledger_range.end() + 1u64;
    let (tx_ledger_range, tx_partition_range) = calculate_tx_ranges(
        start_chunk_offset,
        &partition_1_range,
        bytes_in_tx,
        config.consensus.chunk_size,
    );

    let _ = storage_modules[1].index_transaction_data(
        tx_path.clone(),
        data_root,
        tx_ledger_range,
        data_size,
    );

    let tx_path_hash = H256::from(hash_sha256(tx_path).unwrap());
    verify_tx_path_in_submodule(submodule4, tx_path, tx_path_hash);

    verify_tx_path_offsets(submodule4, tx_path_hash, tx_partition_range, &[]);

    verify_data_root_data_size(submodule4, data_root, data_size);

    // =========================================================================
    // Post Chunks Tests
    // =========================================================================

    // Manually update the data_root -> partition_hash index
    let db = open_or_create_db(tmp_dir, IrysTables::ALL, None).unwrap();
    let _part_hash_0 = storage_modules[0].partition_hash().unwrap();
    let _part_hash_1 = storage_modules[1].partition_hash().unwrap();

    // Loop though all the transactions and add their chunks to the cache
    for tx in &txs {
        let mut prev_byte_offset: u64 = 0;
        info!("num chunks in tx: {:?}", tx.proofs.len());
        for (i, proof) in tx.proofs.iter().enumerate() {
            let chunk_bytes = Base64(
                tx.data.clone().unwrap().0[prev_byte_offset as usize..=proof.offset].to_vec(),
            );

            // verify the chunk length
            assert_eq!(chunk_bytes.len(), config.consensus.chunk_size as usize);

            // verify the chunk hash
            let chunk_hash = hash_sha256(&chunk_bytes.0).unwrap();
            assert_eq!(chunk_hash, tx.chunks[i].data_hash.unwrap());

            let chunk = UnpackedChunk {
                data_root: tx.header.data_root,
                data_size: chunk_bytes.len() as u64,
                data_path: Base64(proof.proof.clone()),
                bytes: chunk_bytes,
                tx_offset: TxChunkOffset::from(
                    TryInto::<u32>::try_into(i).expect("Value exceeds u32::MAX"),
                ),
            };

            let _ = db.update_eyre(|tx| cache_chunk(tx, &chunk));
            prev_byte_offset = proof.offset as u64 + 1; // Update for next iteration
        }
    }

    // Now loop through all the transactions using their data_roots and data_sizes
    // to write data chunks to the storage modules
    let mut ledger_offset: LedgerChunkOffset = LedgerChunkOffset::from(0);
    for tx in &txs {
        let data_root = tx.header.data_root;
        let num_chunks = (tx.header.data_size / config.consensus.chunk_size)
            .try_into()
            .expect("Value exceeds u32::MAX");

        let mut chunks_added = 0;
        // loop though the assigned partitions
        for storage_module in &storage_modules {
            let _ = db.view(|tx| {
                for i in chunks_added..num_chunks {
                    let tx_chunk_offset = TxChunkOffset::from(i);
                    // first make sure the ledger_offset falls within the bounds
                    // of the storage_module. Sometime txs contain ledger relative
                    // chunk_offsets that span multiple storage modules.
                    if !storage_module.contains_offset(ledger_offset) {
                        continue;
                    }

                    // Request the chunk from the global db index by  data root & tx relative offset
                    let res = cached_chunk_by_chunk_offset(tx, data_root, tx_chunk_offset).unwrap();

                    // Build a Chunk struct to store in the submodule
                    if let Some((_metadata, chunk)) = res {
                        let chunk_bytes = chunk.chunk.unwrap();
                        let chunk_byte_value = chunk_bytes.0[0];
                        info!("chunk_bytes: {:?}", chunk_byte_value);
                        let chunk = UnpackedChunk {
                            data_root,
                            data_size: chunk_bytes.len() as u64,
                            data_path: chunk.data_path,
                            bytes: chunk_bytes,
                            tx_offset: tx_chunk_offset,
                        };

                        let res = storage_module.write_data_chunk(&chunk);
                        if let Err(err) = res {
                            panic!("{}", err);
                        }
                    }

                    ledger_offset += 1;
                    chunks_added += 1;
                }
            });

            //storage_module.print_pending_writes();
        }
    }

    // These are synchronous operations
    let _ = storage_modules[0].sync_pending_chunks();
    let _ = storage_modules[1].sync_pending_chunks();

    // For each of the storage modules, makes sure they sync to disk
    let chunks1 = storage_modules[0]
        .read_chunks(partition_chunk_offset_ii!(0, 19))
        .unwrap();
    let chunks2 = storage_modules[1]
        .read_chunks(partition_chunk_offset_ii!(0, 19))
        .unwrap();

    // This is just helpful logging, could be commented out
    for i in 0..=19 {
        if let Some((chunk, chunk_type)) = chunks1.get(&PartitionChunkOffset::from(i)) {
            let preview = &chunk[..chunk.len().min(5)];
            info!(
                "storage_module[0][{:?}]: {:?}... - {:?}",
                i, preview, chunk_type
            );
        } else {
            info!("storage_module[0][{:?}]: None", i);
        }
    }
    for i in 0..=19 {
        if let Some((chunk, chunk_type)) = chunks2.get(&PartitionChunkOffset::from(i)) {
            let preview = &chunk[..chunk.len().min(5)];
            info!(
                "storage_module[1][{:?}]: {:?}... - {:?}",
                i, preview, chunk_type
            );
        } else {
            info!("storage_module[1][{:?}]: None", i);
        }
    }

    // Test the chunks read back from the storage modules
    for i in 0..=19 {
        if let Some((chunk, chunk_type)) = chunks1.get(&PartitionChunkOffset::from(i)) {
            let bytes = [i as u8; 32];
            assert_eq!(*chunk, bytes);
            assert_eq!(*chunk_type, ChunkType::Data);
            println!("read[sm0]: {:?}", chunk);
        }
    }

    for i in 0..=19 {
        if let Some((chunk, chunk_type)) = chunks2.get(&PartitionChunkOffset::from(i)) {
            let bytes = [20 + i as u8; 32];
            if i <= 7 {
                assert_eq!(*chunk, bytes);
                assert_eq!(*chunk_type, ChunkType::Data);
            } else {
                assert_eq!(*chunk_type, ChunkType::Entropy)
            }
            println!("read[sm1]: {:?}", chunk);
        }
    }
    Ok(())
}

fn hash_sha256(message: &[u8]) -> Result<[u8; 32], eyre::Error> {
    let mut hasher = sha::Sha256::new();
    hasher.update(message);
    let result = hasher.finish();
    Ok(result)
}

fn verify_tx_path_in_submodule(submodule: &StorageSubmodule, tx_path: &[u8], tx_path_hash: H256) {
    submodule
        .db
        .view(|tx| {
            let path = get_full_tx_path(tx, tx_path_hash)
                .unwrap()
                .expect("tx_path bytes not found in index");
            assert_eq!(path, tx_path);
        })
        .unwrap();
}

fn verify_tx_path_offsets(
    submodule: &StorageSubmodule,
    tx_path_hash: H256,
    chunk_range: PartitionChunkRange,
    expected_missing_offsets: &[u32],
) {
    submodule
        .db
        .view(|tx| {
            for offset in *chunk_range.0.start()..=*chunk_range.0.end() {
                match get_path_hashes_by_offset(tx, PartitionChunkOffset::from(offset)).unwrap() {
                    Some(paths) => {
                        let tx_ph = paths
                            .tx_path_hash
                            .expect("index exists but tx_path_hash value is empty");
                        assert_eq!(tx_path_hash, tx_ph);
                    }
                    None => {
                        // Assert this offset should be missing
                        assert!(
                            expected_missing_offsets.contains(&offset),
                            "Unexpected missing offset {} - should be one of {:?}",
                            offset,
                            expected_missing_offsets
                        );
                    }
                }
            }
        })
        .unwrap();
}

fn calculate_tx_ranges(
    start_chunk_offset: LedgerChunkOffset,
    partition_range: &LedgerChunkRange,
    bytes_in_tx: u64,
    chunk_size: u64,
) -> (LedgerChunkRange, PartitionChunkRange) {
    let mut num_chunks_in_tx = bytes_in_tx / chunk_size;

    let ledger_range = LedgerChunkRange(ie(
        start_chunk_offset,
        start_chunk_offset + num_chunks_in_tx,
    ));

    let partition_start = *start_chunk_offset as i64 - *partition_range.start() as i64;
    if partition_start < 0 {
        num_chunks_in_tx = (num_chunks_in_tx as i64 + partition_start) as u64;
    }

    let partition_start: u32 = partition_start
        .max(0)
        .try_into()
        .expect("Value exceeds u32::MAX");

    let partition_range = PartitionChunkRange(partition_chunk_offset_ie!(
        partition_start,
        partition_start + num_chunks_in_tx as u32
    ));

    (ledger_range, partition_range)
}
fn verify_data_root_start_offset(
    submodule: &StorageSubmodule,
    data_root: H256,
    expected_offset: i32,
) {
    submodule
        .db
        .view(|tx| {
            let relative_start_offsets = get_start_offsets_by_data_root(tx, data_root)
                .unwrap()
                .expect("start offsets not found");
            assert_eq!(relative_start_offsets.0.len(), 1);
            assert_eq!(relative_start_offsets.0[0], expected_offset.into());
        })
        .unwrap();
}

fn verify_data_root_data_size(submodule: &StorageSubmodule, data_root: H256, expected_size: u64) {
    assert_eq!(
        submodule
            .db
            .view_eyre(|tx| get_data_size_by_data_root(tx, data_root))
            .unwrap(),
        Some(expected_size)
    );
}
