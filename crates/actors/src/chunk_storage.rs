use actix::prelude::*;
use irys_config::chain::StorageConfig;
use irys_database::{submodule::add_full_tx_path, BlockIndex, Initialized, Ledger};
use irys_storage::{ii, InclusiveInterval, StorageModule};
use irys_types::{
    Address, DataRoot, Interval, IrysBlockHeader, IrysTransactionHeader, Proof, TransactionLedger,
    H256, NUM_PARTITIONS_PER_SLOT,
};
use openssl::sha;
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, RwLock},
};
use tracing::error;

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
    pub miner_address: Address,
    /// Global block index for block bounds/offset tracking
    pub block_index: Arc<RwLock<BlockIndex<Initialized>>>,
    pub storage_config: Arc<StorageConfig>,
    pub epoch_service_addr: Addr<EpochServiceActor>,
    pub storage_modules: Vec<Arc<StorageModule>>,
}

impl Actor for ChunkStorageActor {
    type Context = Context<Self>;
}

impl ChunkStorageActor {
    /// Creates a new chunk storage actor
    pub fn new(
        miner_address: Address,
        block_index: Arc<RwLock<BlockIndex<Initialized>>>,
        storage_config: Arc<StorageConfig>,
        epoch_service_addr: Addr<EpochServiceActor>,
        storage_modules: Vec<Arc<StorageModule>>,
    ) -> Self {
        Self {
            miner_address,
            block_index,
            storage_config,
            epoch_service_addr,
            storage_modules,
        }
    }
}

fn get_block_range(
    block_header: &IrysBlockHeader,
    ledger: Ledger,
    block_index: Arc<RwLock<BlockIndex<Initialized>>>,
) -> Interval<u64> {
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

    let block_offsets = ii(
        start_chunk_offset,
        block_header.ledgers[ledger].max_chunk_offset,
    );

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
    chunk_range: Interval<u64>,
    miner_address: Address,
    epoch_service: Addr<EpochServiceActor>,
    storage_modules: &Vec<Arc<StorageModule>>,
) -> eyre::Result<Vec<(&Arc<StorageModule>, usize)>> {
    // Get all the PartitionAssignments that are overlapped by this transactions chunks
    let assignments = epoch_service
        .send(GetOverlappingPartitionsMessage {
            ledger: Ledger::Submit,
            chunk_range,
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

    let module_pairs: Vec<_> = storage_modules
        .iter()
        .filter_map(|module| {
            module.partition_hash.map(|hash| {
                assignments
                    .iter()
                    .find(|(assign_hash, _)| assign_hash == &hash)
                    .map(|(_, slot_idx)| (module, (*slot_idx).unwrap()))
            })
        })
        .flatten()
        .collect();

    if assignments.len() != module_pairs.len() {
        return Err(eyre::eyre!(
            "Unable to find storage module for assigned partition hash"
        ));
    }

    Ok(module_pairs)
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
        let miner_address = self.miner_address;
        let storage_modules = self.storage_modules.clone();
        let num_chunks_in_partition = self.storage_config.num_chunks_in_partition;

        // Async move closure to call async methods withing a non async fn
        Box::pin(async move {
            let path_pairs = get_tx_path_pairs(&block_header, &txs).unwrap();
            let ledger_relative_block_range =
                get_block_range(&block_header, Ledger::Submit, block_index.clone());

            // loop though each tx_path and set up all the storage module indexes
            let mut prev_byte_offset = 0;
            let mut ledger_relative_prev_chunk_offset = ledger_relative_block_range.start();

            for (tx_path, data_root) in path_pairs {
                let tx_path_hash = H256::from(hash_sha256(&tx_path.proof).unwrap());

                // Calculate chunks in this tx path segment
                let tx_byte_length = tx_path.offset - prev_byte_offset;
                let num_chunks_in_tx = (tx_byte_length / chunk_size) as u64;

                // Calculate the ledger relative chunk range for this transaction
                let ledger_relative_tx_range = ii(
                    ledger_relative_prev_chunk_offset,
                    ledger_relative_prev_chunk_offset + num_chunks_in_tx,
                );

                // Retrieve the storage modules that are overlapped by this range
                let matching_modules = get_overlapping_storage_modules(
                    ledger_relative_tx_range,
                    miner_address,
                    epoch_service.clone(),
                    &storage_modules,
                )
                .await
                .unwrap();

                // Add the tx_path to each of the modules indexes
                for (module, slot_index) in matching_modules {
                    // Range of the StorageModule in the ledger
                    let start = slot_index as u64 * num_chunks_in_partition;
                    let ledger_relative_module_range = ii(start, start + num_chunks_in_partition);

                    // Compute a module relative tx overlap range
                    // TODO: only ever use module relative offsets inside modules
                    let overlap_range = ledger_relative_tx_range
                        .intersection(&ledger_relative_module_range)
                        .unwrap();

                    // Transform the ledger relative overlap range to module relative
                    let module_relative_chunk_range = ii(
                        (overlap_range.start() - ledger_relative_module_range.start()) as u32,
                        (overlap_range.end() - ledger_relative_module_range.start()) as u32,
                    );

                    if let Err(e) = module.add_tx_path_to_index(
                        tx_path_hash,
                        tx_path.proof.clone(),
                        module_relative_chunk_range,
                    ) {
                        error!("Failed to add tx path to index: {}", e);
                        return Err(());
                    }

                    // TODO: better here for overlapping modules
                    let start_offset = ledger_relative_prev_chunk_offset as i32;
                    module.add_start_offset_by_data_root(data_root, start_offset);
                }
                prev_byte_offset = tx_path.offset;
                ledger_relative_prev_chunk_offset += num_chunks_in_tx;
            }

            // // loop though all the chunk_offsets added by this block
            for i in ledger_relative_block_range.start()..=ledger_relative_block_range.end() {
                //
                //		- get their chunk path hash keys
                //
                //		- attempt to retrieve the chunk bytes from the mempool and add them to the storage module
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
