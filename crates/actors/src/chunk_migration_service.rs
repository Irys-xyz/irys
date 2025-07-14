use actix::prelude::*;
use eyre::eyre;
use irys_database::{
    cached_chunk_by_chunk_offset,
    db::IrysDatabaseExt as _,
    db_cache::{CachedChunk, CachedChunkIndexMetadata},
};
use irys_domain::BlockIndex;
use irys_storage::{
    get_overlapped_storage_modules, ie, ii, InclusiveInterval as _, StorageModule,
    StorageModulesReadGuard,
};
use irys_types::{
    app_state::DatabaseProvider, Base64, Config, DataLedger, DataRoot, DataTransactionLedger,
    IrysBlockHeader, IrysTransactionHeader, LedgerChunkOffset, LedgerChunkRange, Proof,
    TxChunkOffset, UnpackedChunk, H256,
};
use std::sync::{Arc, RwLock};
use tracing::{error, instrument};

use crate::services::Stop;
use crate::{
    block_producer::BlockFinalizedMessage, cache_service::CacheServiceAction,
    services::ServiceSenders,
};

/// Central coordinator for chunk storage operations.
///
/// Responsibilities:
/// - Routes chunks to appropriate storage modules
/// - Maintains chunk location indices
/// - Coordinates chunk reads/writes
/// - Manages storage state transitions
#[derive(Debug)]
pub struct ChunkMigrationService {
    /// Tracks block boundaries and offsets for locating chunks in ledgers
    pub block_index: Option<Arc<RwLock<BlockIndex>>>,
    /// Configuration parameters for storage system
    pub config: Config,
    /// Collection of storage modules for distributing chunk data
    pub storage_modules_guard: StorageModulesReadGuard,
    /// Persistent database for storing chunk metadata and indices
    pub db: Option<DatabaseProvider>,
    /// Service sender channels
    pub service_senders: Option<ServiceSenders>,
}

impl Default for ChunkMigrationService {
    fn default() -> Self {
        unimplemented!("don't rely on `Default` impl");
    }
}

impl Actor for ChunkMigrationService {
    type Context = Context<Self>;
}

impl ChunkMigrationService {
    pub fn new(
        block_index: Arc<RwLock<BlockIndex>>,
        config: Config,
        storage_modules_guard: &StorageModulesReadGuard,
        db: DatabaseProvider,
        service_senders: ServiceSenders,
    ) -> Self {
        println!("service started: chunk_migration");
        Self {
            block_index: Some(block_index),
            config,
            storage_modules_guard: storage_modules_guard.clone(),
            db: Some(db),
            service_senders: Some(service_senders),
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
        let chunk_size = self.config.consensus.chunk_size as usize;
        let storage_modules = Arc::new(self.storage_modules_guard.clone());
        let db = Arc::new(self.db.clone().unwrap());
        let service_senders = self.service_senders.clone().unwrap();

        // Extract transactions for each ledger
        let submit_tx_count = block.data_ledgers[DataLedger::Submit].tx_ids.len();
        let submit_txs = all_txs[..submit_tx_count].to_vec();
        let publish_txs = all_txs[submit_tx_count..].to_vec();
        let block_height = block.height;
        Box::pin(async move {
            // Process Submit ledger transactions
            process_ledger_transactions(
                &block,
                DataLedger::Submit,
                &submit_txs,
                &block_index,
                chunk_size,
                &storage_modules,
                &db,
            )
            // TODO: fix this & child functions so they forward errors?
            .map_err(|()| eyre!("Unexpected error processing submit ledger transactions"))?;

            // Process Publish ledger transactions
            process_ledger_transactions(
                &block,
                DataLedger::Publish,
                &publish_txs,
                &block_index,
                chunk_size,
                &storage_modules,
                &db,
            )
            .map_err(|()| eyre!("Unexpected error processing publish ledger transactions"))?;

            // forward the finalization message to the cache service for cleanup
            let _ = service_senders
                .chunk_cache
                .send(CacheServiceAction::OnFinalizedBlock(block_height, None));

            Ok(())
        })
    }
}

impl Handler<Stop> for ChunkMigrationService {
    type Result = ();

    fn handle(&mut self, _msg: Stop, ctx: &mut Self::Context) {
        ctx.stop();
    }
}

fn process_ledger_transactions(
    block: &Arc<IrysBlockHeader>,
    ledger: DataLedger,
    txs: &[IrysTransactionHeader],
    block_index: &Arc<RwLock<BlockIndex>>,
    chunk_size: usize,
    storage_modules_guard: &StorageModulesReadGuard,
    db: &Arc<DatabaseProvider>,
) -> Result<(), ()> {
    let path_pairs = get_tx_path_pairs(block, ledger, txs).unwrap();
    let block_range = get_block_range(block, ledger, block_index.clone());
    let mut prev_chunk_offset = block_range.start();

    for ((_txid, tx_path), (data_root, data_size)) in path_pairs {
        let num_chunks_in_tx: u32 = data_size
            .div_ceil(chunk_size as u64)
            .try_into()
            .expect("Value exceeds u32::MAX");
        let tx_chunk_range = LedgerChunkRange(ie(
            prev_chunk_offset,
            prev_chunk_offset + num_chunks_in_tx as u64,
        ));

        update_storage_module_indexes(
            &tx_path.proof,
            data_root,
            tx_chunk_range,
            ledger,
            storage_modules_guard,
            data_size,
        )?;

        process_transaction_chunks(
            num_chunks_in_tx,
            data_root,
            data_size,
            tx_chunk_range,
            ledger,
            storage_modules_guard,
            db,
        )?;

        for module in storage_modules_guard.read().iter() {
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
    ledger: DataLedger,
    storage_modules_guard: &StorageModulesReadGuard,
    db: &DatabaseProvider,
) -> Result<(), ()> {
    for tx_chunk_offset in 0..num_chunks_in_tx {
        let tx_chunk_offset = TxChunkOffset::from(tx_chunk_offset);
        // Attempt to retrieve the cached chunk from the mempool
        let chunk_info = match get_cached_chunk(db, data_root, tx_chunk_offset) {
            Ok(Some(info)) => info,
            _ => continue,
        };

        // Find which storage module intersects this chunk
        let ledger_offset = tx_chunk_range.start() + *tx_chunk_offset;
        let storage_module =
            find_storage_module(storage_modules_guard, ledger, ledger_offset.into());

        // Write the chunk data to the Storage Module
        if let Some(module) = storage_module {
            write_chunk_to_module(&module, chunk_info, data_root, data_size, tx_chunk_offset)?;
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
    ledger: DataLedger,
    block_index: Arc<RwLock<BlockIndex>>,
) -> LedgerChunkRange {
    // Use the block index to get the ledger relative chunk offset of the
    // start of this new block from the previous block.
    let index_reader = block_index.read().unwrap();
    let start_chunk_offset = if block.height > 0 {
        let prev_item = index_reader.get_item(block.height - 1).unwrap();
        prev_item.ledgers[ledger].max_chunk_offset
    } else {
        0
    };

    LedgerChunkRange(ii(
        LedgerChunkOffset::from(start_chunk_offset),
        LedgerChunkOffset::from(block.data_ledgers[ledger].max_chunk_offset),
    ))
}

#[instrument(skip_all, err, fields(block_hash = %block.block_hash, height = %block.height))]
fn get_tx_path_pairs(
    block: &IrysBlockHeader,
    ledger: DataLedger,
    txs: &[IrysTransactionHeader],
) -> eyre::Result<Vec<((H256, Proof), (DataRoot, u64))>> {
    let (tx_root, proofs) = DataTransactionLedger::merklize_tx_root(txs);

    let block_tx_root = block.data_ledgers[ledger].tx_root;
    if tx_root != block_tx_root {
        return Err(eyre::eyre!(
            "Invalid tx_root for {:?} ledger - expected {} got {} ",
            &ledger,
            &tx_root,
            &block_tx_root
        ));
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
    ledger: DataLedger,
    storage_modules_guard: &StorageModulesReadGuard,
    data_size: u64,
) -> Result<(), ()> {
    let overlapped_modules =
        get_overlapped_storage_modules(storage_modules_guard, ledger, &tx_chunk_range);

    for storage_module in overlapped_modules {
        storage_module
            .index_transaction_data(proof.to_vec(), data_root, tx_chunk_range, data_size)
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
    storage_modules_guard: &StorageModulesReadGuard,
    ledger: DataLedger,
    ledger_offset: u64,
) -> Option<Arc<StorageModule>> {
    // Return Arc<StorageModule> (not a reference)
    let guard = storage_modules_guard.read();

    guard.iter().find_map(|module| {
        // First check ledger
        module
            .partition_assignment()
            .as_ref()
            .and_then(|pa| pa.ledger_id)
            .filter(|&id| id == ledger as u32)
            // Then check offset range
            .and_then(|_| module.get_storage_module_ledger_range().ok())
            .filter(|range| range.contains_point(ledger_offset.into()))
            .map(|_| module.clone()) // Clone the Arc here (it's cheap)
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
