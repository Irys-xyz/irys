use std::{
    fs::remove_dir_all,
    str::FromStr,
    sync::{Arc, RwLock},
    time::{SystemTime, UNIX_EPOCH},
};

use {
    irys_actors::block_index::{BlockIndexActor, GetBlockHeightMessage},
    irys_actors::block_producer::BlockConfirmedMessage,
    irys_actors::mempool::{ChunkIngressMessage, MempoolActor, TxIngressMessage},
};

use assert_matches::assert_matches;

use actix::prelude::*;
use chunk::TxRelativeChunkIndex;
use irys_actors::{block_producer::BlockFinalizedMessage, chunk_storage::ChunkStorageActor};
use irys_config::IrysNodeConfig;
use irys_database::{open_or_create_db, tables::IrysTables, BlockIndex, Initialized};
use irys_storage::*;
use irys_testing_utils::utils::setup_tracing_and_temp_dir;
use irys_types::{
    app_state::DatabaseProvider, chunk, irys::IrysSigner, partition::*, Address, Base64, UnpackedChunk,
    H256List, IrysBlockHeader, IrysSignature, IrysTransaction, IrysTransactionHeader, PoaData,
    Signature, StorageConfig, TransactionLedger, H256, U256,
};
use reth::{revm::primitives::B256, tasks::TaskManager};
use tracing::info;

#[actix::test]
async fn finalize_block_test() -> eyre::Result<()> {
    let node_config = IrysNodeConfig::default();
    if node_config.base_directory.exists() {
        remove_dir_all(&node_config.base_directory)?;
    }

    // Create a storage config for testing
    let storage_config = StorageConfig {
        chunk_size: 32,
        num_chunks_in_partition: 6,
        num_chunks_in_recall_range: 2,
        num_partitions_in_slot: 1,
        miner_address: Address::random(),
        min_writes_before_sync: 1,
        entropy_packing_iterations: 1,
    };
    let chunk_size = storage_config.chunk_size;

    // Create StorageModules for testing
    let storage_module_infos = vec![
        StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment {
                partition_hash: H256::random(),
                miner_address: storage_config.miner_address,
                ledger_num: Some(1),
                slot_index: Some(0), // Submit Ledger Slot 0
            }),
            submodules: vec![
                (ii(0, 5), "sm1".to_string()), // 0 to 5 inclusive
            ],
        },
        StorageModuleInfo {
            id: 1,
            partition_assignment: Some(PartitionAssignment {
                partition_hash: H256::random(),
                miner_address: storage_config.miner_address,
                ledger_num: Some(1),
                slot_index: Some(1), // Submit Ledger Slot 1
            }),
            submodules: vec![
                (ii(0, 5), "sm2".to_string()), // 0 to 5 inclusive
            ],
        },
    ];

    let tmp_dir = setup_tracing_and_temp_dir(Some("chunk_storage_test"), false);
    let base_path = tmp_dir.path().to_path_buf();
    info!("temp_dir:{:?}\nbase_path:{:?}", tmp_dir, base_path);
    let _ = initialize_storage_files(&base_path, &storage_module_infos);

    // Create a Vec initialized storage modules
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

    // Create a bunch of TX chunks
    let data_chunks = vec![
        vec![[0; 32], [1; 32], [2; 32]], // Fill most of one Partition
        vec![[3; 32], [4; 32], [5; 32]], // Overlap the next Partition
        vec![[6; 32], [7; 32], [8; 32]], // Fill most of the Partition
    ];

    // Create a bunch of signed TX from the chunks
    // Loop though all the data_chunks and create wrapper tx for them
    let signer = IrysSigner::random_signer_with_chunk_size(chunk_size as usize);
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

    // Create vectors of tx headers and txids
    let tx_headers: Vec<IrysTransactionHeader> = txs.iter().map(|tx| tx.header.clone()).collect();
    let data_tx_ids = tx_headers
        .iter()
        .map(|h| h.id.clone())
        .collect::<Vec<H256>>();
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();

    // Create a block_index
    let arc_config = Arc::new(node_config);
    let block_index: Arc<RwLock<BlockIndex<Initialized>>> = Arc::new(RwLock::new(
        BlockIndex::default()
            .reset(&arc_config.clone())?
            .init(arc_config.clone())
            .await
            .unwrap(),
    ));

    // Create an instance of the mempool actor
    let task_manager = TaskManager::current();
    let db = open_or_create_db(tmp_dir, IrysTables::ALL, None).unwrap();
    let arc_db1 = DatabaseProvider(Arc::new(db));
    let mempool_actor = MempoolActor::new(
        arc_db1.clone(),
        task_manager.executor(),
        arc_config.mining_signer.clone(),
        storage_config.clone(),
        storage_modules.clone(),
    );
    let mempool_addr = mempool_actor.start();

    // Send tx headers to mempool
    tx_headers.iter().for_each(|header| {
        mempool_addr.do_send(TxIngressMessage(header.clone()));
    });

    // Send chunks to mempool
    for tx in &txs {
        for (i, (proof, chunk)) in tx.proofs.iter().zip(&tx.chunks).enumerate() {
            let chunk = UnpackedChunk {
                data_root: tx.header.data_root,
                data_size: tx.header.data_size,
                data_path: Base64::from(proof.proof.clone()),
                bytes: Base64(tx.data.0[chunk.min_byte_range..chunk.max_byte_range].to_vec()),
                chunk_index: i as TxRelativeChunkIndex,
            };

            let res = mempool_addr
                .send(ChunkIngressMessage(chunk))
                .await?
                .map_err(|e| panic!("{:?}", e));

            assert_matches!(res, Ok(()));
        }
    }

    // Create a block_index actor
    let block_index_actor = BlockIndexActor::new(block_index.clone());
    let block_index_addr = block_index_actor.start();

    // Get the blockheight from the actor
    let height = block_index_addr
        .send(GetBlockHeightMessage {})
        .await
        .unwrap();

    for tx in txs.iter() {
        println!("data_root: {:?}", tx.header.data_root.0);
    }

    // Create a block from the tx
    let irys_block = IrysBlockHeader {
        diff: U256::from(1000),
        cumulative_diff: U256::from(5000),
        last_retarget: 1622543200,
        solution_hash: H256::zero(),
        previous_solution_hash: H256::zero(),
        last_epoch_hash: H256::random(),
        chunk_hash: H256::zero(),
        height,
        block_hash: H256::zero(),
        previous_block_hash: H256::zero(),
        previous_cumulative_diff: U256::from(4000),
        poa: PoaData {
            tx_path: Base64::from_str("").unwrap(),
            data_path: Base64::from_str("").unwrap(),
            chunk: Base64::from_str("").unwrap(),
        },
        reward_address: Address::ZERO,
        reward_key: Base64::from_str("").unwrap(),
        signature: IrysSignature {
            reth_signature: Signature::test_signature(),
        },
        timestamp: now.as_millis() as u64,
        ledgers: vec![
            // Permanent Publish Ledger
            TransactionLedger {
                tx_root: H256::zero(),
                txids: H256List(Vec::new()),
                max_chunk_offset: 0,
                expires: None,
            },
            // Term Submit Ledger
            TransactionLedger {
                tx_root: TransactionLedger::merklize_tx_root(&tx_headers).0,
                txids: H256List(data_tx_ids.clone()),
                max_chunk_offset: 0,
                expires: Some(1622543200),
            },
        ],
        evm_block_hash: B256::ZERO,
    };

    // Send the block confirmed message
    let block = Arc::new(irys_block);
    let txs = Arc::new(tx_headers);
    let block_confirm_message = BlockConfirmedMessage(block.clone(), Arc::clone(&txs));

    block_index_addr.do_send(block_confirm_message.clone());

    // Send the block finalized message
    let chunk_storage_actor = ChunkStorageActor::new(
        block_index.clone(),
        storage_config.clone(),
        storage_modules.clone(),
        arc_db1.clone(),
    );
    let chunk_storage_addr = chunk_storage_actor.start();
    let block_finalized_message = BlockFinalizedMessage {
        block_header: block.clone(),
        txs: txs.clone(),
    };
    let _res = chunk_storage_addr.send(block_finalized_message).await?;

    // Check to see if the chunks are in the StorageModules
    for sm in storage_modules.iter() {
        let _ = sm.sync_pending_chunks();
    }

    // For each of the storage modules, makes sure they sync to disk
    let chunks1 = storage_modules[0].read_chunks(ii(0, 5)).unwrap();
    let chunks2 = storage_modules[1].read_chunks(ii(0, 5)).unwrap();

    for i in 0..=5 {
        if let Some((chunk, chunk_type)) = chunks1.get(&i) {
            let preview = &chunk[..chunk.len().min(5)];
            info!(
                "storage_module[0][{:?}]: {:?}... - {:?}",
                i, preview, chunk_type
            );
        } else {
            info!("storage_module[0][{:?}]: None", i);
        }
    }
    for i in 0..=5 {
        if let Some((chunk, chunk_type)) = chunks2.get(&i) {
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
    for i in 0..=5 {
        if let Some((chunk, chunk_type)) = chunks1.get(&i) {
            let bytes = [i as u8; 32];
            assert_eq!(*chunk, bytes);
            assert_eq!(*chunk_type, ChunkType::Data);
            println!("read[sm0]: {:?}", chunk);
        }
    }

    for i in 0..=5 {
        if let Some((chunk, chunk_type)) = chunks2.get(&i) {
            let bytes = [6 + i as u8; 32];
            if i <= 2 {
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
