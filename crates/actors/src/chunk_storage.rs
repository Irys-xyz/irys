use actix::prelude::*;
use irys_database::{
    cached_chunk_by_chunk_index, submodule::add_full_tx_path, BlockIndex, Initialized, Ledger,
};
use irys_storage::{ie, ii, InclusiveInterval, StorageModule};
use irys_types::{
    app_state::DatabaseProvider, chunk, Address, Base64, Chunk, DataRoot, Interval,
    IrysBlockHeader, IrysTransactionHeader, LedgerChunkOffset, LedgerChunkRange, Proof,
    StorageConfig, TransactionLedger, H256,
};
use openssl::sha;
use reth_db::Database;
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, RwLock},
};
use tracing::{error, info};

use crate::{
    block_producer::BlockFinalizedMessage,
    epoch_service::{self, EpochServiceActor, GetOverlappingPartitionsMessage},
};

/// Central coordinator for chunk storage operations.
///
/// Responsibilities:
/// - Routes chunks to appropriate storage modules
/// - Maintains chunk location indices
/// - Coordinates chunk reads/writes
/// - Manages storage state transitions
#[derive(Debug)]
pub struct ChunkStorageActor {
    /// Global block index for block bounds/offset tracking
    pub block_index: Arc<RwLock<BlockIndex<Initialized>>>,
    pub storage_config: StorageConfig,
    pub storage_modules: Vec<Arc<StorageModule>>,
    pub db: DatabaseProvider,
}

impl Actor for ChunkStorageActor {
    type Context = Context<Self>;
}

impl ChunkStorageActor {
    /// Creates a new chunk storage actor
    pub fn new(
        block_index: Arc<RwLock<BlockIndex<Initialized>>>,
        storage_config: StorageConfig,
        storage_modules: Vec<Arc<StorageModule>>,
        db: DatabaseProvider,
    ) -> Self {
        Self {
            block_index,
            storage_config,
            storage_modules,
            db,
        }
    }
}

fn get_block_range(
    block_header: &IrysBlockHeader,
    ledger: Ledger,
    block_index: Arc<RwLock<BlockIndex<Initialized>>>,
) -> LedgerChunkRange {
    // Use the block index to get the ledger relative chunk offset of the
    // start of this new block from the previous block.
    let index_reader = block_index.read().unwrap();
    let start_chunk_offset = if block_header.height > 0 {
        let prev_item = index_reader
            .get_item(block_header.height as usize - 1)
            .unwrap();
        prev_item.ledgers[ledger].max_chunk_offset
    } else {
        0
    };

    let block_offsets = LedgerChunkRange(ii(
        start_chunk_offset,
        block_header.ledgers[ledger].max_chunk_offset,
    ));

    block_offsets
}
fn get_tx_path_pairs(
    block_header: &IrysBlockHeader,
    txs: &Vec<IrysTransactionHeader>,
) -> eyre::Result<Vec<(Proof, DataRoot)>> {
    // Changed Proof to TxPath
    let (tx_root, proofs) = TransactionLedger::merklize_tx_root(txs);
    if tx_root != block_header.ledgers[Ledger::Submit].tx_root {
        return Err(eyre::eyre!("Invalid tx_root"));
    }

    Ok(proofs
        .into_iter()
        .zip(txs.iter().map(|tx| tx.data_root))
        .collect())
}

impl Handler<BlockFinalizedMessage> for ChunkStorageActor {
    type Result = ResponseFuture<Result<(), ()>>;

    fn handle(&mut self, msg: BlockFinalizedMessage, _: &mut Context<Self>) -> Self::Result {
        // Collect working variables to move into the closure
        let block_header = msg.block_header;
        let txs = msg.txs;
        let block_index = self.block_index.clone();
        let chunk_size = self.storage_config.chunk_size as usize;
        let storage_modules = self.storage_modules.clone();
        let db = self.db.clone();

        info!("BlockHeader: {:#?}", block_header);

        // Async move closure to call async methods withing a non async fn
        Box::pin(async move {
            let path_pairs = get_tx_path_pairs(&block_header, &txs).unwrap();
            let block_range = get_block_range(&block_header, Ledger::Submit, block_index.clone());

            let mut prev_byte_offset = 0;
            let mut prev_chunk_offset: LedgerChunkOffset = block_range.start();

            // loop though each tx_path and add entries to the indexes in the storage modules
            // overlapped by the tx_path's chunks
            for (tx_path, data_root) in path_pairs {
                // Calculate the number of chunks added to the ledger by this transaction
                let tx_byte_length = tx_path.offset - prev_byte_offset;
                let num_chunks_in_tx = tx_byte_length.div_ceil(chunk_size) as u32;

                // Calculate the ledger relative chunk range for this transaction
                let tx_chunk_range = LedgerChunkRange(ie(
                    prev_chunk_offset,
                    prev_chunk_offset + num_chunks_in_tx as u64,
                ));

                // Retrieve the storage modules that are overlapped by this range
                let matching_modules: Vec<_> = storage_modules
                    .iter()
                    .filter(|module| {
                        module
                            .partition_assignment
                            .and_then(|pa| pa.ledger_num)
                            .map_or(false, |num| num == Ledger::Submit as u64)
                            && module
                                .get_storage_module_range()
                                .map_or(false, |range| range.overlaps(&tx_chunk_range))
                    })
                    .collect();

                // Update each of the affected StorageModules
                for storage_module in matching_modules {
                    // Store the tx_path_hash and its path bytes
                    if let Err(e) = storage_module.index_transaction_data(
                        tx_path.proof.clone(),
                        data_root,
                        tx_chunk_range,
                    ) {
                        error!(
                            "Failed to add tx path + data_root + start_offset to index: {}",
                            e
                        );
                        return Err(());
                    }
                }

                // Loop through transaction's chunks
                for chunk_index in 0..num_chunks_in_tx {
                    // Get chunk from the global cache
                    if let Ok(Some(chunk_info)) =
                        db.view_eyre(|tx| cached_chunk_by_chunk_index(tx, data_root, chunk_index))
                    {
                        // Compute the ledger relative chunk_offset
                        let ledger_offset = chunk_index as u64 + tx_chunk_range.start();

                        // Grab the correct storage module for the offset
                        let matching_module = storage_modules.iter().find_map(|module| {
                            module
                                .get_storage_module_range()
                                .ok()
                                .filter(|range| range.contains_point(ledger_offset))
                                .map(|_| module)
                        });

                        // Add the cached chunk to the StorageModule index and disk
                        if let Some(storage_module) = matching_module {
                            let data_path = Base64::from(chunk_info.1.data_path.0.clone());
                            if let Some(bytes) = chunk_info.1.chunk {
                                // Create a Chunk struct
                                let chunk = Chunk {
                                    data_root,
                                    data_size: tx_byte_length as u64,
                                    data_path,
                                    bytes,
                                    chunk_index: chunk_index,
                                };

                                // Write the chunk to the module
                                if let Err(e) = storage_module.write_data_chunk(&chunk) {
                                    error!("Failed to write data chunk: {}", e);
                                    return Err(());
                                }
                            }
                        }
                    }
                }

                prev_byte_offset = tx_path.offset;
                prev_chunk_offset += num_chunks_in_tx as u64;
            }

            Ok(())
        })
    }
}

fn hash_sha256(message: &[u8]) -> Result<[u8; 32], eyre::Error> {
    let mut hasher = sha::Sha256::new();
    hasher.update(message);
    let result = hasher.finish();
    Ok(result)
}

mod tests {
    use std::{
        fs::remove_dir_all,
        str::FromStr,
        time::{SystemTime, UNIX_EPOCH},
    };

    use crate::{
        block_index::{BlockIndexActor, GetBlockHeightMessage},
        block_producer::BlockConfirmedMessage,
        mempool::{ChunkIngressMessage, MempoolActor, TxIngressMessage},
    };

    use super::*;
    use actix::prelude::*;
    use chunk::TxRelativeChunkIndex;
    use irys_config::IrysNodeConfig;
    use irys_database::{open_or_create_db, tables::IrysTables};
    use irys_storage::*;
    use irys_testing_utils::utils::setup_tracing_and_temp_dir;
    use irys_types::{
        irys::IrysSigner, partition::*, H256List, IrysSignature, IrysTransaction, PoaData,
        Signature, U256,
    };
    use reth::{revm::primitives::B256, tasks::TaskManager};

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

        let mut storage_modules: Vec<Arc<StorageModule>> = Vec::new();

        // Create a Vec initialized storage modules
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

        let tx_headers: Vec<IrysTransactionHeader> =
            txs.iter().map(|tx| tx.header.clone()).collect();

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

        // Add the tx headers the mempool actor
        for tx_header in tx_headers.iter() {
            let msg = TxIngressMessage(tx_header.clone());
            mempool_addr.do_send(msg);
        }

        // Add the chunks to the mempool actor
        for tx in txs.iter() {
            for (i, proof) in tx.proofs.iter().enumerate() {
                let offset = proof.offset as u64;
                let min = tx.chunks[i].min_byte_range;
                let max = tx.chunks[i].max_byte_range;
                let chunk = Chunk {
                    data_root: tx.header.data_root,
                    data_size: tx.header.data_size,
                    data_path: Base64::from(proof.proof.clone()),
                    bytes: Base64(tx.data.0[min..max].to_vec()),
                    chunk_index: i as TxRelativeChunkIndex,
                };
                let msg = ChunkIngressMessage(chunk);
                let res = mempool_addr.send(msg).await?;
                if let Err(err) = res {
                    panic!("{:?}", err);
                }
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
        let mut irys_block = IrysBlockHeader {
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
                ledger_num: 0,
                partition_chunk_offset: 0,
                partition_hash: H256::zero(),
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
        let res = chunk_storage_addr.send(block_finalized_message).await?;

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
}
