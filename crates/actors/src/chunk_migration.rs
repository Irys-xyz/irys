use actix::prelude::*;
use irys_database::{
    cached_chunk_by_chunk_offset,
    db_cache::{CachedChunk, CachedChunkIndexMetadata},
    BlockIndex, Initialized, Ledger,
};
use irys_storage::{get_overlapped_storage_modules, ie, ii, InclusiveInterval, StorageModule};
use irys_types::{
    app_state::DatabaseProvider, Base64, DataRoot, IrysBlockHeader, IrysTransactionHeader,
    LedgerChunkOffset, LedgerChunkRange, Proof, StorageConfig, TransactionLedger,
    TxRelativeChunkOffset, UnpackedChunk,
};
use reth_db::Database;
use std::sync::{Arc, RwLock};
use tracing::error;

use crate::block_producer::BlockFinalizedMessage;

/// Central coordinator for chunk storage operations.
///
/// Responsibilities:
/// - Routes chunks to appropriate storage modules
/// - Maintains chunk location indices
/// - Coordinates chunk reads/writes
/// - Manages storage state transitions
#[derive(Debug)]
pub struct ChunkMigrationActor {
    /// Tracks block boundaries and offsets for locating chunks in ledgers
    pub block_index: Arc<RwLock<BlockIndex<Initialized>>>,
    /// Configuration parameters for storage system
    pub storage_config: StorageConfig,
    /// Collection of storage modules for distributing chunk data
    pub storage_modules: Vec<Arc<StorageModule>>,
    /// Persistent database for storing chunk metadata and indices
    pub db: DatabaseProvider,
}

impl Actor for ChunkMigrationActor {
    type Context = Context<Self>;
}

impl ChunkMigrationActor {
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

impl Handler<BlockFinalizedMessage> for ChunkMigrationActor {
    type Result = ResponseFuture<Result<(), ()>>;

    fn handle(&mut self, msg: BlockFinalizedMessage, _: &mut Context<Self>) -> Self::Result {
        // Collect working variables to move into the closure
        let block_header = msg.block_header;
        let txs = msg.txs;
        let block_index = self.block_index.clone();
        let chunk_size = self.storage_config.chunk_size as usize;
        let storage_modules = self.storage_modules.clone();
        let db = self.db.clone();

        // info!("Finalized BlockHeader: {:#?}", block_header);

        // Async move closure to call async methods withing a non async fn
        Box::pin(async move {
            let path_pairs = get_tx_path_pairs(&block_header, &txs).unwrap();
            let block_range = get_block_range(&block_header, Ledger::Submit, block_index.clone());

            let mut prev_chunk_offset: LedgerChunkOffset = block_range.start();

            // loop though each tx_path and add entries to the indexes in the storage modules
            // overlapped by the tx_path's chunks
            for (tx_path, (data_root, data_size)) in path_pairs {
                let num_chunks_in_tx = data_size.div_ceil(chunk_size as u64) as u32;

                // Calculate the ledger relative chunk range for this transaction
                let tx_chunk_range = LedgerChunkRange(ie(
                    prev_chunk_offset,
                    prev_chunk_offset + num_chunks_in_tx as u64,
                ));

                // Retrieve the storage modules that are overlapped by this range
                let overlapped_modules = get_overlapped_storage_modules(
                    &storage_modules,
                    Ledger::Submit,
                    &tx_chunk_range,
                );

                // Update the transaction indexes of the overlapped StorageModules
                update_storage_module_indexes(
                    &overlapped_modules,
                    &tx_path.proof,
                    data_root,
                    tx_chunk_range,
                )?;

                // Process the transaction's chunks, getting them from the cache
                // and writing them to the correct StorageModule
                process_transaction_chunks(
                    num_chunks_in_tx,
                    data_root,
                    data_size,
                    tx_chunk_range,
                    &storage_modules,
                    &db,
                )?;

                for module in storage_modules.iter() {
                    let _ = module.sync_pending_chunks();
                }

                prev_chunk_offset += num_chunks_in_tx as u64;
            }

            Ok(())
        })
    }
}

fn process_transaction_chunks(
    num_chunks_in_tx: u32,
    data_root: DataRoot,
    data_size: u64,
    tx_chunk_range: LedgerChunkRange,
    storage_modules: &[Arc<StorageModule>],
    db: &DatabaseProvider,
) -> Result<(), ()> {
    for tx_chunk_offset in 0..num_chunks_in_tx {
        // Attempt to retrieve the cached chunk from the mempool
        let chunk_info = match get_cached_chunk(db, data_root, tx_chunk_offset) {
            Ok(Some(info)) => info,
            _ => continue,
        };

        // Find which storage module intersects this chunk
        let ledger_offset = tx_chunk_offset as u64 + tx_chunk_range.start();
        let storage_module = find_storage_module(storage_modules, Ledger::Submit, ledger_offset);

        // Write the chunk data to the Storage Module
        if let Some(module) = storage_module {
            write_chunk_to_module(module, chunk_info, data_root, data_size, tx_chunk_offset)?;
        }
    }
    Ok(())
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
) -> eyre::Result<Vec<(Proof, (DataRoot, u64))>> {
    // Changed Proof to TxPath
    let (tx_root, proofs) = TransactionLedger::merklize_tx_root(txs);
    if tx_root != block_header.ledgers[Ledger::Submit].tx_root {
        return Err(eyre::eyre!("Invalid tx_root"));
    }

    Ok(proofs
        .into_iter()
        .zip(txs.iter().map(|tx| (tx.data_root, tx.data_size)))
        .collect())
}

fn update_storage_module_indexes(
    matching_modules: &[Arc<StorageModule>],
    proof: &[u8],
    data_root: DataRoot,
    tx_chunk_range: LedgerChunkRange,
) -> Result<(), ()> {
    for storage_module in matching_modules {
        storage_module
            .index_transaction_data(proof.to_vec(), data_root, tx_chunk_range)
            .map_err(|e| {
                error!(
                    "Failed to add tx path + data_root + start_offset to index: {}",
                    e
                );
                ()
            })?;
    }
    Ok(())
}
fn get_cached_chunk(
    db: &DatabaseProvider,
    data_root: DataRoot,
    chunk_offset: TxRelativeChunkOffset,
) -> eyre::Result<Option<(CachedChunkIndexMetadata, CachedChunk)>> {
    db.view_eyre(|tx| cached_chunk_by_chunk_offset(tx, data_root, chunk_offset))
}

fn find_storage_module(
    storage_modules: &[Arc<StorageModule>],
    ledger: Ledger,
    ledger_offset: u64,
) -> Option<&Arc<StorageModule>> {
    storage_modules.iter().find_map(|module| {
        // First check ledger
        module
            .partition_assignment
            .as_ref()
            .and_then(|pa| pa.ledger_num)
            .filter(|&num| num == ledger as u64)
            // Then check offset range
            .and_then(|_| module.get_storage_module_range().ok())
            .filter(|range| range.contains_point(ledger_offset))
            .map(|_| module)
    })
}

fn write_chunk_to_module(
    storage_module: &Arc<StorageModule>,
    chunk_info: (CachedChunkIndexMetadata, CachedChunk),
    data_root: DataRoot,
    data_size: u64,
    chunk_offset: TxRelativeChunkOffset,
) -> Result<(), ()> {
    let data_path = Base64::from(chunk_info.1.data_path.0.clone());

    if let Some(bytes) = chunk_info.1.chunk {
        let chunk = UnpackedChunk {
            data_root,
            data_size,
            data_path,
            bytes,
            tx_offset: chunk_offset,
        };

        storage_module.write_data_chunk(&chunk).map_err(|e| {
            error!("Failed to write data chunk: {}", e);
            ()
        })?;
    }
    Ok(())
}
