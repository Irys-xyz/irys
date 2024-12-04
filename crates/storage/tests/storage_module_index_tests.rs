use std::sync::Arc;

use irys_database::submodule::{
    get_full_tx_path, get_path_hashes_by_offset, get_start_offsets_by_data_root,
};
use irys_storage::*;
use irys_testing_utils::utils::setup_tracing_and_temp_dir;
use irys_types::{
    irys::IrysSigner, partition::PartitionAssignment, Address, IrysTransaction,
    IrysTransactionHeader, LedgerChunkOffset, LedgerChunkRange, PartitionChunkRange, StorageConfig,
    TransactionLedger, H256,
};
use openssl::sha;
use reth_db::Database;
use tracing::info;

#[test]
fn tx_path_overlap_tests() {
    // Set up the storage geometry for this test
    let storage_config = StorageConfig {
        chunk_size: 32,
        num_chunks_in_partition: 20,
        num_chunks_in_recall_range: 5,
        num_partitions_in_slot: 1,
        miner_address: Address::random(),
        min_writes_before_sync: 1,
        ..Default::default()
    };
    let chunk_size = storage_config.chunk_size;

    // Configure 3 storage modules that are assigned to the submit ledger in
    // slots 0, 1, and 2
    let storage_module_infos = vec![
        StorageModuleInfo {
            module_num: 0,
            partition_assignment: Some(PartitionAssignment {
                partition_hash: H256::random(),
                miner_address: storage_config.miner_address,
                ledger_num: Some(1),
                slot_index: Some(0), // Submit Ledger Slot 0
            }),
            submodules: vec![
                (ii(0, 4), "hdd0".to_string()),   // 0 to 4 inclusive
                (ii(5, 9), "hdd1".to_string()),   // 5 to 9 inclusive
                (ii(10, 19), "hdd2".to_string()), // 10 to 19 inclusive
            ],
        },
        StorageModuleInfo {
            module_num: 1,
            partition_assignment: Some(PartitionAssignment {
                partition_hash: H256::random(),
                miner_address: storage_config.miner_address,
                ledger_num: Some(1),
                slot_index: Some(1), // Submit Ledger Slot 1
            }),
            submodules: vec![
                (ii(0, 9), "hdd3".to_string()),   // 0 to 9 inclusive
                (ii(10, 19), "hdd4".to_string()), // 10 to 19 inclusive
            ],
        },
        StorageModuleInfo {
            module_num: 2,
            partition_assignment: Some(PartitionAssignment {
                partition_hash: H256::random(),
                miner_address: storage_config.miner_address,
                ledger_num: Some(1),
                slot_index: Some(2), // Submit Ledger Slot 2
            }),
            submodules: vec![
                (ii(0, 19), "hdd5".to_string()), // 0 to 19 inclusive
            ],
        },
    ];

    let tmp_dir = setup_tracing_and_temp_dir(Some("storage_module_test"), false);
    let base_path = tmp_dir.path().to_path_buf();
    info!("temp_dir:{:?}\nbase_path:{:?}", tmp_dir, base_path);

    let _ = initialize_storage_files(&base_path, &storage_module_infos);

    let mut storage_modules: Vec<Arc<StorageModule>> = Vec::new();

    // Create a Vec initialized storage modules
    for info in storage_module_infos {
        let arc_module = Arc::new(StorageModule::new(
            &base_path,
            &info,
            Some(storage_config.clone()),
        ));
        storage_modules.push(arc_module.clone());
    }

    let partition_0_range = LedgerChunkRange(ii(0, 19));
    let partition_1_range = LedgerChunkRange(ii(20, 39));

    // Create a list of BLOBs that represent transaction data
    let data_chunks = vec![
        vec![[b'a'; 32], [b'b'; 32], [b'c'; 32]], // Fill most of one submodule
        vec![[b'd'; 32], [b'e'; 32], [b'f'; 32]], // Overlap the next submodule
        vec![
            [b'g'; 32], [b'h'; 32], [b'i'; 32], [b'j'; 32], [b'k'; 32], [b'l'; 32], [b'm'; 32],
            [b'n'; 32], [b'o'; 32], [b'p'; 32], [b'q'; 32], [b'r'; 32], [b's'; 32],
        ], // Stop one short of filling the StorageModule
        vec![[b't'; 32], [b'u'; 32], [b'v'; 32]], // Overlap the next StorageModule
        vec![
            [b'w'; 32], [b'x'; 32], [b'y'; 32], [b'z'; 32], [b'1'; 32], [b'2'; 32],
        ], // Perfectly fills the submodule without overlapping
    ];

    // Loop though all the data_chunks and create wrapper tx for them
    let signer = IrysSigner::random_signer();
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
    let (_tx_root, proofs) = TransactionLedger::merklize_tx_root(&tx_headers);

    // Assume this is the first block in the blockchain
    let proof = &proofs[0];
    let tx_path = &proof.proof;

    // Tx:1 - Base case, write tx index data without any overlaps
    let num_chunks_in_tx = (proof.offset + 1) as u64 / storage_config.chunk_size;
    let (tx_ledger_range, tx_partition_range) =
        calculate_tx_ranges(0, &partition_0_range, proof.offset as u64, chunk_size);

    let data_root = tx_headers[0].data_root;
    let _ = storage_modules[0].index_transaction_data(tx_path.to_vec(), data_root, tx_ledger_range);

    // Get the submodule reference
    let submodule = storage_modules[0]
        .get_submodule(0)
        .ok_or(eyre::eyre!("Storage module not found"))
        .unwrap();

    // Verify the tx_path_hash and tx_path bytes were added
    let tx_path_hash = H256::from(hash_sha256(&tx_path).unwrap());
    verify_tx_path_in_submodule(submodule, &tx_path, tx_path_hash);

    verify_tx_path_offsets(&submodule, tx_path_hash, tx_partition_range, &[]);

    verify_data_root_start_offset(submodule, data_root, 0);

    // Tx:2 - Overlapping case, tx chunks start in one submodule and go to another
    let start_chunk_offset = num_chunks_in_tx;
    let bytes_in_tx = proofs[1].offset as u64 - proof.offset as u64;
    let (tx_ledger_range, tx_partition_range) = calculate_tx_ranges(
        start_chunk_offset,
        &partition_0_range,
        bytes_in_tx,
        chunk_size,
    );
    let tx_path = &proofs[1].proof;
    let data_root = tx_headers[1].data_root;
    let _ = storage_modules[0].index_transaction_data(tx_path.to_vec(), data_root, tx_ledger_range);

    // Get the both submodule references
    let submodule = storage_modules[0]
        .get_submodule(0)
        .ok_or(eyre::eyre!("Storage module not found"))
        .unwrap();

    let submodule2 = storage_modules[0]
        .get_submodule(tx_partition_range.end())
        .ok_or(eyre::eyre!("Storage module not found"))
        .unwrap();

    // Verify the tx_path_hash and tx_path bytes were added to both submodules
    let tx_path_hash = H256::from(hash_sha256(&tx_path).unwrap());

    verify_tx_path_in_submodule(submodule, &tx_path, tx_path_hash);
    verify_tx_path_in_submodule(submodule2, &tx_path, tx_path_hash);

    verify_tx_path_offsets(&submodule, tx_path_hash, tx_partition_range, &[5]);
    verify_tx_path_offsets(&submodule2, tx_path_hash, tx_partition_range, &[3, 4]);

    verify_data_root_start_offset(submodule, data_root, 3);
    verify_data_root_start_offset(submodule2, data_root, 3);

    // Tx:3 - Fill up the StorageModule leaving one empty chunk
    let tx_path = &proofs[2].proof;
    let data_root = tx_headers[2].data_root;
    let bytes_in_tx = proofs[2].offset as u64 - (tx_ledger_range.end() * storage_config.chunk_size);
    let start_chunk_offset = tx_ledger_range.end() + 1;
    let (tx_ledger_range, tx_partition_range) = calculate_tx_ranges(
        start_chunk_offset,
        &partition_0_range,
        bytes_in_tx,
        chunk_size,
    );
    let _ = storage_modules[0].index_transaction_data(tx_path.to_vec(), data_root, tx_ledger_range);

    let submodule3 = storage_modules[0]
        .get_submodule(tx_partition_range.end())
        .ok_or(eyre::eyre!("Storage module not found"))
        .unwrap();

    // Verify the tx_path hash and bytes were added to the 2nd submodule
    let tx_path_hash = H256::from(hash_sha256(&tx_path).unwrap());
    verify_tx_path_in_submodule(submodule2, &tx_path, tx_path_hash);
    verify_tx_path_in_submodule(submodule3, &tx_path, tx_path_hash);

    verify_tx_path_offsets(
        &submodule2,
        tx_path_hash,
        tx_partition_range,
        &[10, 11, 12, 13, 14, 15, 16, 17, 18],
    );
    verify_tx_path_offsets(
        &submodule3,
        tx_path_hash,
        tx_partition_range,
        &[6, 7, 8, 9, 10],
    );

    verify_data_root_start_offset(submodule2, data_root, 6);
    verify_data_root_start_offset(submodule3, data_root, 6);

    // Tx:4 - Overlap between StorageModules
    let tx_path = &proofs[3].proof;
    let data_root = tx_headers[3].data_root;
    let offset = proofs[3].offset as u64;
    let bytes_in_tx = (offset + 1) - ((tx_ledger_range.end() + 1) * storage_config.chunk_size);
    let start_chunk_offset = tx_ledger_range.end() + 1;
    let (tx_ledger_range, tx_partition_range) = calculate_tx_ranges(
        start_chunk_offset,
        &partition_0_range,
        bytes_in_tx,
        chunk_size,
    );
    // Update both storage modules with the tx data
    let _ = storage_modules[0].index_transaction_data(tx_path.to_vec(), data_root, tx_ledger_range);
    let _ = storage_modules[1].index_transaction_data(tx_path.to_vec(), data_root, tx_ledger_range);

    // The first submodule of the second StorageModule/Partition
    let submodule4 = storage_modules[1]
        .get_submodule(0)
        .ok_or(eyre::eyre!("Storage module not found"))
        .unwrap();

    // Verify the tx_path hash and bytes were added to the 2nd submodule
    let tx_path_hash = H256::from(hash_sha256(&tx_path).unwrap());
    verify_tx_path_in_submodule(submodule3, &tx_path, tx_path_hash);
    verify_tx_path_in_submodule(submodule4, &tx_path, tx_path_hash);

    verify_tx_path_offsets(&submodule3, tx_path_hash, tx_partition_range, &[20, 21]);

    // We now need ranges relative to the second partition
    let (_tx_ledger_range, tx_partition_range) = calculate_tx_ranges(
        start_chunk_offset,
        &partition_1_range,
        bytes_in_tx,
        chunk_size,
    );

    verify_tx_path_offsets(&submodule4, tx_path_hash, tx_partition_range, &[]);
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
            for offset in chunk_range.start()..=chunk_range.end() {
                match get_path_hashes_by_offset(tx, offset as u32).unwrap() {
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

    let partition_start = (start_chunk_offset as i64 - partition_range.start() as i64);
    if partition_start < 0 {
        num_chunks_in_tx = (num_chunks_in_tx as i64 + partition_start) as u64;
    }

    let partition_start = partition_start.max(0) as u32;

    let partition_range = PartitionChunkRange(ie(
        partition_start,
        partition_start + num_chunks_in_tx as u32,
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
            assert_eq!(relative_start_offsets.0[0], expected_offset);
        })
        .unwrap();
}
