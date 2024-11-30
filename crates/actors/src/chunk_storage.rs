use actix::prelude::*;
use irys_config::chain::StorageConfig;
use irys_database::{
    cached_chunk_by_offset, submodule::add_full_tx_path, BlockIndex, Initialized, Ledger,
};
use irys_storage::{ii, InclusiveInterval, StorageModule};
use irys_types::{
    app_state::DatabaseProvider, Address, Chunk, DataRoot, Interval, IrysBlockHeader,
    IrysTransactionHeader, LedgerChunkOffset, LedgerChunkRange, Proof, TransactionLedger, H256,
};
use openssl::sha;
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
    pub storage_config: Arc<StorageConfig>,
    pub epoch_service_addr: Addr<EpochServiceActor>,
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
        storage_config: Arc<StorageConfig>,
        epoch_service_addr: Addr<EpochServiceActor>,
        storage_modules: Vec<Arc<StorageModule>>,
        db: DatabaseProvider,
    ) -> Self {
        Self {
            block_index,
            storage_config,
            epoch_service_addr,
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

async fn get_overlapping_storage_modules(
    chunk_range: LedgerChunkRange,
    miner_address: Address,
    epoch_service: Addr<EpochServiceActor>,
    storage_modules: &Vec<Arc<StorageModule>>,
) -> eyre::Result<Vec<&Arc<StorageModule>>> {
    // Get all the PartitionAssignments that are overlapped by this transactions chunks
    let assignments = epoch_service
        .send(GetOverlappingPartitionsMessage {
            ledger: Ledger::Submit,
            chunk_range: chunk_range.into(),
        })
        .await
        .unwrap();

    // Get the first partition assignment at each slot index that matches
    // our mining address
    let filtered_assignments: Vec<_> = assignments
        .into_iter()
        .filter(|a| a.miner_address == miner_address && a.slot_index.is_some())
        .fold(HashMap::new(), |mut map, assignment| {
            map.entry(assignment.slot_index.unwrap())
                .or_insert(assignment);
            map
        })
        .into_values()
        .collect();

    // Get the storage modules mapped to these partitions
    let assignments: HashSet<(H256, Option<usize>)> = filtered_assignments
        .iter()
        .map(|a| (a.partition_hash, a.slot_index))
        .collect();

    let modules: Vec<_> = storage_modules
        .iter()
        .filter(|module| {
            let hash = module.partition_hash().unwrap();
            assignments
                .iter()
                .any(|(assign_hash, _)| assign_hash == &hash)
        })
        .collect();

    if assignments.len() != modules.len() {
        return Err(eyre::eyre!(
            "Unable to find storage module for assigned partition hash"
        ));
    }

    Ok(modules)
}

impl Handler<BlockFinalizedMessage> for ChunkStorageActor {
    type Result = ResponseFuture<Result<(), ()>>;

    fn handle(&mut self, msg: BlockFinalizedMessage, _: &mut Context<Self>) -> Self::Result {
        // Collect working variables to move into the closure
        let block_header = msg.block_header;
        let txs = msg.txs;
        let epoch_service = self.epoch_service_addr.clone();
        let block_index = self.block_index.clone();
        let chunk_size = self.storage_config.chunk_size as usize;
        let miner_address = self.storage_config.miner_address;
        let storage_modules = self.storage_modules.clone();
        let db = self.db.clone();

        // Async move closure to call async methods withing a non async fn
        Box::pin(async move {
            let path_pairs = get_tx_path_pairs(&block_header, &txs).unwrap();
            let block_range = get_block_range(&block_header, Ledger::Submit, block_index.clone());

            let mut prev_byte_offset = 0;
            let mut prev_chunk_offset: LedgerChunkOffset = block_range.start();

            // loop though each tx_path and add entries to the indexes in the storage modules
            // overlapped by the tx_path's chunks
            for (tx_path, data_root) in path_pairs {
                // Compute the tx path hash for each tx_path proof
                let tx_path_hash = H256::from(hash_sha256(&tx_path.proof).unwrap());

                // Calculate the number of chunks added to the ledger by this transaction
                let tx_byte_length = tx_path.offset - prev_byte_offset;
                let num_chunks_in_tx = (tx_byte_length / chunk_size) as u64;

                // Calculate the ledger relative chunk range for this transaction
                let tx_chunk_range =
                    LedgerChunkRange(ii(prev_chunk_offset, prev_chunk_offset + num_chunks_in_tx));

                // Retrieve the storage modules that are overlapped by this range
                let matching_modules = get_overlapping_storage_modules(
                    tx_chunk_range,
                    miner_address,
                    epoch_service.clone(),
                    &storage_modules,
                )
                .await
                .unwrap();

                // Update each of the affected StorageModules
                for storage_module in matching_modules {
                    // Store the tx_path_hash and its path bytes
                    if let Err(e) = storage_module.add_tx_path_to_index(
                        tx_path_hash,
                        tx_path.proof.clone(),
                        tx_chunk_range,
                    ) {
                        error!("Failed to add tx path to index: {}", e);
                        return Err(());
                    }
                    // Update the start_offset index
                    if let Err(e) =
                        storage_module.add_start_offset_by_data_root(data_root, tx_chunk_range)
                    {
                        error!("Failed to add start_offset to index: {}", e);
                        return Err(());
                    }
                }

                // Loop through transaction's chunks
                for chunk_offset in 0..num_chunks_in_tx as u32 {
                    // Get chunk from cache
                    if let Ok(Some(chunk_info)) =
                        cached_chunk_by_offset(&db, data_root, chunk_offset)
                    {
                        // Compute the ledger relative chunk_offset
                        let ledger_offset = chunk_offset as u64 + tx_chunk_range.start();

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
                            // For now, write the data_path for the chunk offset
                            let _ = storage_module.add_data_path_to_index(
                                chunk_info.0.chunk_path_hash,
                                chunk_info.1.data_path.into(),
                                ledger_offset,
                            );
                            info!("cached_chunk found: {:?}", chunk_info.0);
                            // TODO: This doesn't yet take into account packing
                            // let cached_chunk = chunk_info.1;
                            // let _ = storage_module.write_data_chunk(Chunk {
                            //     data_root,
                            //     data_size: chunk_size as u64,
                            //     data_path: cached_chunk.data_path,
                            //     bytes: cached_chunk.chunk.unwrap(),
                            //     offset: chunk_offset,
                            // });
                        }
                    }
                }

                prev_byte_offset = tx_path.offset;
                prev_chunk_offset += num_chunks_in_tx;
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
