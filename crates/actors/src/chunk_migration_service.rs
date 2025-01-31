use actix::prelude::*;
use eyre::eyre;
use irys_database::{
    cached_chunk_by_chunk_offset,
    db_cache::{CachedChunk, CachedChunkIndexMetadata},
    BlockIndex, Initialized, Ledger,
};
use irys_storage::{get_overlapped_storage_modules, ie, ii, InclusiveInterval, StorageModule};
use irys_types::{
    app_state::DatabaseProvider, Base64, DataRoot, IrysBlockHeader, IrysTransactionHeader,
    LedgerChunkRange, Proof, StorageConfig, TransactionLedger, TxChunkOffset,
    UnpackedChunk, H256,
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
#[derive(Debug, Default)]
pub struct ChunkMigrationService {
    /// Tracks block boundaries and offsets for locating chunks in ledgers
    pub block_index: Option<Arc<RwLock<BlockIndex<Initialized>>>>,
    /// Configuration parameters for storage system
    pub storage_config: StorageConfig,
    /// Collection of storage modules for distributing chunk data
    pub storage_modules: Vec<Arc<StorageModule>>,
    /// Persistent database for storing chunk metadata and indices
    pub db: Option<DatabaseProvider>,
}

impl Actor for ChunkMigrationService {
    type Context = Context<Self>;
}

impl ChunkMigrationService {
    pub fn new(
        block_index: Arc<RwLock<BlockIndex<Initialized>>>,
        storage_config: StorageConfig,
        storage_modules: Vec<Arc<StorageModule>>,
        db: DatabaseProvider,
    ) -> Self {
        println!("service started: chunk_migration");
        Self {
            block_index: Some(block_index),
            storage_config,
            storage_modules,
            db: Some(db),
        }
    }
}

/// Adds this actor the the local service registry
impl Supervised for ChunkMigrationService {}

impl SystemService for ChunkMigrationService {
    fn service_started(&mut self, _ctx: &mut Context<Self>) {
        println!("chunk_migration service started");
    }
}

impl Handler<BlockFinalizedMessage> for ChunkMigrationService {
    type Result = ResponseFuture<eyre::Result<()>>;

    fn handle(&mut self, msg: BlockFinalizedMessage, _: &mut Context<Self>) -> Self::Result {
        // Early return if not initialized
        if self.block_index.is_none() || self.db.is_none() {
            error!("chunk_migration service not initialized");
            return Box::pin(async move { Err(eyre!("chunk_migration service not initialized")) });
        }

        // Collect working variables to move into the closure
        let block = msg.block_header;
        let all_txs = msg.all_txs;
        let block_index = self.block_index.clone().unwrap();
        let chunk_size = self.storage_config.chunk_size as usize;
        let storage_modules = Arc::new(self.storage_modules.clone());
        let db = Arc::new(self.db.clone().unwrap());

        // Extract transactions for each ledger
        let submit_tx_count = block.ledgers[Ledger::Submit].tx_ids.len();
        let submit_txs = all_txs[..submit_tx_count].to_vec();
        let publish_txs = all_txs[submit_tx_count..].to_vec();

        Box::pin(async move {
            // Process Submit ledger transactions
            process_ledger_transactions(
                &block,
                Ledger::Submit,
                &submit_txs,
                &block_index,
                chunk_size,
                &storage_modules,
                &db,
            )
            // TODO: fix this & child functions so they forward errors?
            .map_err(|_| eyre!("Unexpected error processing submit ledger transactions"))?;

            // Process Publish ledger transactions
            process_ledger_transactions(
                &block,
                Ledger::Publish,
                &publish_txs,
                &block_index,
                chunk_size,
                &storage_modules,
                &db,
            )
            .map_err(|_| eyre!("Unexpected error processing publish ledger transactions"))?;

            Ok(())
        })
    }
}

fn process_ledger_transactions(
    block: &Arc<IrysBlockHeader>,
    ledger: Ledger,
    txs: &[IrysTransactionHeader],
    block_index: &Arc<RwLock<BlockIndex<Initialized>>>,
    chunk_size: usize,
    storage_modules: &Arc<Vec<Arc<StorageModule>>>,
    db: &Arc<DatabaseProvider>,
) -> Result<(), ()> {
    let path_pairs = get_tx_path_pairs(block, ledger, txs).unwrap();
    let block_range = get_block_range(block, ledger, block_index.clone());
    let mut prev_chunk_offset = block_range.start();

    for ((_txid, tx_path), (data_root, data_size)) in path_pairs {
        let num_chunks_in_tx = data_size.div_ceil(chunk_size as u64) as u32;
        let tx_chunk_range = LedgerChunkRange(ie(
            prev_chunk_offset,
            prev_chunk_offset + num_chunks_in_tx as u64,
        ));

        update_storage_module_indexes(
            &tx_path.proof,
            data_root,
            tx_chunk_range,
            ledger,
            storage_modules,
        )?;

        process_transaction_chunks(
            num_chunks_in_tx,
            data_root,
            data_size,
            tx_chunk_range,
            ledger,
            storage_modules,
            db,
        )?;

        for module in storage_modules.iter() {
            let _ = module.sync_pending_chunks();
        }

        prev_chunk_offset += num_chunks_in_tx as u64;
    }

    Ok(())
}

fn process_transaction_chunks(
    num_chunks_in_tx: u32,
    data_root: DataRoot,
    data_size: u64,
    tx_chunk_range: LedgerChunkRange,
    ledger: Ledger,
    storage_modules: &[Arc<StorageModule>],
    db: &DatabaseProvider,
) -> Result<(), ()> {
    for tx_chunk_offset in 0..num_chunks_in_tx {
        // Attempt to retrieve the cached chunk from the mempool
        let chunk_info = match get_cached_chunk(db, data_root, tx_chunk_offset.into()) {
            Ok(Some(info)) => info,
            _ => continue,
        };

        // Find which storage module intersects this chunk
        let ledger_offset = tx_chunk_offset as u64 + tx_chunk_range.start();
        let storage_module = find_storage_module(storage_modules, ledger, ledger_offset);

        // Write the chunk data to the Storage Module
        if let Some(module) = storage_module {
            write_chunk_to_module(module, chunk_info, data_root, data_size, tx_chunk_offset.into())?;
        }
    }
    Ok(())
}

/// Computes the range of chunks added to a ledger by the transactions in a block,
/// relative to the ledger.
///
/// The calculation starts from the previous block's `max_chunk_offset` (or 0 for genesis)
/// for the given ledger and extends to this block's `max_chunk_offset` within the same ledger.
///
/// # Arguments
/// * `block_header` - The block header containing height and ledger information.
/// * `ledger` - The target ledger (e.g., Submit or Publish).
/// * `block_index` - Index of historical block data.
///
/// # Returns
/// A `LedgerChunkRange` representing the [start, end] chunk offsets of the chunks
/// added to the ledger by the specified block.

fn get_block_range(
    block: &IrysBlockHeader,
    ledger: Ledger,
    block_index: Arc<RwLock<BlockIndex<Initialized>>>,
) -> LedgerChunkRange {
    // Use the block index to get the ledger relative chunk offset of the
    // start of this new block from the previous block.
    let index_reader = block_index.read().unwrap();
    let start_chunk_offset = if block.height > 0 {
        let prev_item = index_reader.get_item(block.height as usize - 1).unwrap();
        prev_item.ledgers[ledger].max_chunk_offset
    } else {
        0
    };

    LedgerChunkRange(ii(
        start_chunk_offset,
        block.ledgers[ledger].max_chunk_offset,
    ))
}
fn get_tx_path_pairs(
    block: &IrysBlockHeader,
    ledger: Ledger,
    txs: &[IrysTransactionHeader],
) -> eyre::Result<Vec<((H256, Proof), (DataRoot, u64))>> {
    let (tx_root, proofs) = TransactionLedger::merklize_tx_root(txs);

    if tx_root != block.ledgers[ledger].tx_root {
        return Err(eyre::eyre!("Invalid tx_root"));
    }

    Ok(proofs
        .into_iter()
        .zip(txs.iter())
        .map(|(proof, tx)| ((tx.id, proof), (tx.data_root, tx.data_size)))
        .collect())
}

fn update_storage_module_indexes(
    proof: &[u8],
    data_root: DataRoot,
    tx_chunk_range: LedgerChunkRange,
    ledger: Ledger,
    storage_modules: &[Arc<StorageModule>],
) -> Result<(), ()> {
    let overlapped_modules =
        get_overlapped_storage_modules(storage_modules, ledger, &tx_chunk_range);

    for storage_module in overlapped_modules {
        storage_module
            .index_transaction_data(proof.to_vec(), data_root, tx_chunk_range)
            .map_err(|e| {
                error!(
                    "Failed to add tx path + data_root + start_offset to index: {}",
                    e
                );
            })?;
    }
    Ok(())
}
fn get_cached_chunk(
    db: &DatabaseProvider,
    data_root: DataRoot,
    chunk_offset: TxChunkOffset,
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
            .and_then(|pa| pa.ledger_id)
            .filter(|&id| id == ledger as u32)
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
    chunk_offset: TxChunkOffset,
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
        })?;
    }
    Ok(())
}
