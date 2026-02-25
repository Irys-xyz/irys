use crate::{cache_service::CacheServiceAction, services::ServiceSenders};
use irys_database::{
    block_header_by_hash, cached_chunk_by_chunk_offset,
    db::IrysDatabaseExt as _,
    db_cache::{CachedChunk, CachedChunkIndexMetadata},
    tx_header_by_txid,
};
use irys_domain::{
    get_overlapped_storage_modules, BlockIndex, StorageModule, StorageModulesReadGuard,
};
use irys_storage::{ie, ii, InclusiveInterval as _};
use irys_types::{
    app_state::DatabaseProvider, Base64, BlockHash, Config, DataLedger, DataRoot,
    DataTransactionHeader, DataTransactionLedger, IrysBlockHeader, LedgerChunkOffset,
    LedgerChunkRange, Proof, SendTraced as _, TokioServiceHandle, Traced, TxChunkOffset,
    UnpackedChunk, H256,
};
use reth::tasks::shutdown::Shutdown;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{mpsc::UnboundedReceiver, oneshot};
use tracing::{error, instrument};

pub struct ChunkMigrationService {
    shutdown: Shutdown,
    msg_rx: UnboundedReceiver<Traced<ChunkMigrationServiceMessage>>,
    inner: ChunkMigrationServiceInner,
}

/// Central coordinator for chunk storage operations.
///
/// Responsibilities:
/// - Routes chunks to appropriate storage modules
/// - Maintains chunk location indices
/// - Coordinates chunk reads/writes
/// - Manages storage state transitions
#[derive(Debug)]
pub struct ChunkMigrationServiceInner {
    /// Tracks block boundaries and offsets for locating chunks in ledgers
    pub block_index: BlockIndex,
    /// Configuration parameters for storage system
    pub config: Config,
    /// Collection of storage modules for distributing chunk data
    pub storage_modules_guard: StorageModulesReadGuard,
    /// Persistent database for storing chunk metadata and indices
    pub db: DatabaseProvider,
    /// Service sender channels
    pub service_senders: ServiceSenders,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MigrationError {
    /// Failed to write chunk data to submodule
    #[error("Failed to write chunk data to submodule")]
    ChunkDataWrite,
    /// Failed to write chunk index data to submodule database
    #[error("Failed to write chunk index data to submodule database")]
    ChunkIndexWrite,
    /// Catch-all variant for other errors.
    #[error("Ingress proof error: {0}")]
    Other(String),
}

pub enum ChunkMigrationServiceMessage {
    BlockMigrated(
        Arc<IrysBlockHeader>,
        Arc<HashMap<DataLedger, Vec<DataTransactionHeader>>>,
    ),
    UpdateStorageModuleIndexes {
        block_hash: BlockHash,
        receiver: oneshot::Sender<Result<(), MigrationError>>,
    },
}

impl ChunkMigrationServiceInner {
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn new(
        block_index: BlockIndex,
        storage_modules_guard: &StorageModulesReadGuard,
        db: DatabaseProvider,
        service_senders: ServiceSenders,
        config: Config,
    ) -> Self {
        tracing::info!("service started: chunk_migration");
        Self {
            block_index,
            config,
            storage_modules_guard: storage_modules_guard.clone(),
            db,
            service_senders,
        }
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    pub fn handle_message(&mut self, msg: ChunkMigrationServiceMessage) -> eyre::Result<()> {
        match msg {
            ChunkMigrationServiceMessage::BlockMigrated(block_header, all_txs) => {
                self.on_block_migrated(block_header, all_txs)?;
            }
            ChunkMigrationServiceMessage::UpdateStorageModuleIndexes {
                block_hash,
                receiver,
            } => {
                let response_value = self.on_update_storage_module_indexes(block_hash);
                if let Err(e) = receiver.send(response_value) {
                    tracing::error!(
                        "UpdateStorageModuleIndexes receiver.send() error for block {}: {:?}",
                        block_hash,
                        e
                    );
                };
            }
        }
        Ok(())
    }

    fn on_update_storage_module_indexes(
        &mut self,
        block_hash: BlockHash,
    ) -> Result<(), MigrationError> {
        // Retrieve the block from the db
        let block_header = self
            .db
            .view_eyre(|tx| block_header_by_hash(tx, &block_hash, false))
            .expect("db query to succeed")
            .expect("header should exist in db");

        // For each data ledger, retrieve the tx headers and build a map
        let data_ledger_txids = block_header.get_data_ledger_tx_ids();

        let mut block_tx_map: HashMap<DataLedger, Vec<DataTransactionHeader>> = HashMap::new();
        for (ledger, tx_ids) in data_ledger_txids {
            let mut txs = Vec::new();
            for txid in tx_ids {
                let tx = self
                    .db
                    .view_eyre(|tx| tx_header_by_txid(tx, &txid))
                    .unwrap()
                    .unwrap();
                txs.push(tx);
            }
            block_tx_map.insert(ledger, txs);
        }

        // Invoke on_block_migrated to sync the indexes, this will also migrate any available chunks on hand
        self.on_block_migrated(Arc::new(block_header), Arc::new(block_tx_map))?;

        Ok(())
    }

    #[instrument(skip_all, fields(
        height = %block_header.height,
        hash = %block_header.block_hash
    ))]

    fn on_block_migrated(
        &mut self,
        block_header: Arc<IrysBlockHeader>,
        all_txs: Arc<HashMap<DataLedger, Vec<DataTransactionHeader>>>,
    ) -> Result<(), MigrationError> {
        // Collect working variables to move into the closure
        let block = block_header;
        let block_index = self.block_index.clone();
        let chunk_size = self.config.consensus.chunk_size as usize;
        let storage_modules = Arc::new(self.storage_modules_guard.clone());
        let db = Arc::new(self.db.clone());
        let service_senders = self.service_senders.clone();

        // Extract transactions for each ledger
        let submit_txs = all_txs.get(&DataLedger::Submit);
        let publish_txs = all_txs.get(&DataLedger::Publish);
        let block_height = block.height;

        // Process Submit ledger transactions
        if let Some(submit_txs) = submit_txs {
            process_ledger_transactions(
                &block,
                DataLedger::Submit,
                submit_txs,
                &block_index,
                chunk_size,
                &storage_modules,
                &db,
            )?
        }

        // Process Publish ledger transactions
        if let Some(publish_txs) = publish_txs {
            process_ledger_transactions(
                &block,
                DataLedger::Publish,
                publish_txs,
                &block_index,
                chunk_size,
                &storage_modules,
                &db,
            )?
        }

        // forward the finalization message to the cache service for cleanup
        if let Err(e) = service_senders
            .chunk_cache
            .send_traced(CacheServiceAction::OnBlockMigrated(block_height, None))
        {
            tracing::warn!(
                block.height = ?block_height,
                "Failed to send block migrated message to cache service: {}",
                e
            );
        }

        Ok(())
    }
}

#[tracing::instrument(level = "trace", skip_all, err)]
pub fn process_ledger_transactions(
    block: &Arc<IrysBlockHeader>,
    ledger: DataLedger,
    txs: &[DataTransactionHeader],
    block_index: &BlockIndex,
    chunk_size: usize,
    storage_modules_guard: &StorageModulesReadGuard,
    db: &Arc<DatabaseProvider>,
) -> Result<(), MigrationError> {
    let path_pairs = get_tx_path_pairs(block, ledger, txs).unwrap();
    let block_offsets = get_block_offsets_in_ledger(block, ledger, block_index);
    let mut prev_chunk_offset = block_offsets.start();

    for ((_txid, tx_path), tx) in path_pairs {
        let num_chunks_in_tx: u32 = tx
            .data_size
            .div_ceil(chunk_size as u64)
            .try_into()
            .expect("Value exceeds u32::MAX");

        let tx_chunk_range = LedgerChunkRange(ie(
            prev_chunk_offset,
            prev_chunk_offset + num_chunks_in_tx as u64,
        ));

        update_storage_module_indexes(
            tx,
            &tx_path.proof,
            tx_chunk_range,
            ledger,
            storage_modules_guard,
        )?;

        process_transaction_chunks(
            tx,
            num_chunks_in_tx,
            tx_chunk_range,
            ledger,
            storage_modules_guard,
            db,
        )?;

        for module in storage_modules_guard.read().iter() {
            if let Err(e) = module.sync_pending_chunks() {
                tracing::warn!("Failed to sync pending chunks: {:#}", e);
            }
        }

        prev_chunk_offset += num_chunks_in_tx as u64;
    }

    Ok(())
}

fn process_transaction_chunks(
    tx: &DataTransactionHeader,
    num_chunks_in_tx: u32,
    tx_chunk_range: LedgerChunkRange,
    ledger: DataLedger,
    storage_modules_guard: &StorageModulesReadGuard,
    db: &DatabaseProvider,
) -> Result<(), MigrationError> {
    for tx_chunk_offset in 0..num_chunks_in_tx {
        let tx_chunk_offset = TxChunkOffset::from(tx_chunk_offset);
        // Attempt to retrieve the cached chunk from the mempool
        let chunk_info = match get_cached_chunk(db, tx.data_root, tx_chunk_offset) {
            Ok(Some(info)) => info,
            _ => continue,
        };

        // Find which storage module intersects this chunk
        let ledger_offset = tx_chunk_range.start() + *tx_chunk_offset;
        let storage_module =
            find_storage_module(storage_modules_guard, ledger, ledger_offset.into());

        // Write the chunk data to the Storage Module
        if let Some(module) = storage_module {
            write_chunk_to_module(&module, chunk_info, tx, tx_chunk_offset)?;
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
#[tracing::instrument(level = "trace", skip_all, fields(block.height = block.height, ledger = ?ledger))]
fn get_block_offsets_in_ledger(
    block: &IrysBlockHeader,
    ledger: DataLedger,
    block_index: &BlockIndex,
) -> LedgerChunkRange {
    // Use the block index to get the ledger relative chunk offset of the
    // start of this new block from the previous block.
    let start_chunk_offset = if block.height > 0 {
        // We subtract 1 from `total_chunks` to get the offsets
        block_index
            .get_item(block.height - 1)
            .map(|prev| prev.ledgers[ledger].total_chunks)
            .unwrap_or(0)
    } else {
        0
    };

    // Calculate the end offset, accounting for blocks that add no chunks to the ledger.
    // If chunks were added: end_offset = total_chunks - 1 (convert count to 0-indexed offset)
    // If no chunks added: end_offset = start_offset (creates an empty/invalid range, which
    // correctly signals that this block contributed nothing to the ledger)
    let end_chunk_offset = if block.data_ledgers[ledger].total_chunks > start_chunk_offset {
        block.data_ledgers[ledger].total_chunks.saturating_sub(1)
    } else {
        start_chunk_offset
    };

    // debug!(
    //     "get_block_range - {} {}",
    //     start_chunk_offset, end_chunk_offset
    // );

    LedgerChunkRange(ii(
        LedgerChunkOffset::from(start_chunk_offset),
        LedgerChunkOffset::from(end_chunk_offset),
    ))
}

#[instrument(skip_all, err, fields(block.hash = %block.block_hash, block.height = %block.height))]
fn get_tx_path_pairs<'a>(
    block: &'a IrysBlockHeader,
    ledger: DataLedger,
    txs: &'a [DataTransactionHeader],
) -> eyre::Result<Vec<((H256, Proof), &'a DataTransactionHeader)>> {
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
        .map(|(proof, tx)| ((tx.id, proof), tx))
        .collect())
}

#[tracing::instrument(level = "trace", skip_all, err)]
fn update_storage_module_indexes(
    data_tx: &DataTransactionHeader,
    tx_path_proof: &[u8],
    tx_chunk_range: LedgerChunkRange,
    ledger: DataLedger,
    storage_modules_guard: &StorageModulesReadGuard,
) -> Result<(), MigrationError> {
    let overlapped_modules =
        get_overlapped_storage_modules(storage_modules_guard, ledger, &tx_chunk_range);

    for storage_module in overlapped_modules {
        storage_module
            .index_transaction_data(data_tx, &tx_path_proof.to_vec(), tx_chunk_range)
            .map_err(|e| {
                error!(
                    "Failed to add tx path + data_root + start_offset to index: {}",
                    e
                );
                MigrationError::ChunkIndexWrite
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
            .and_then(|_| module.get_storage_module_ledger_offsets().ok())
            .filter(|range| range.contains_point(ledger_offset.into()))
            .map(|_| module.clone()) // Clone the Arc here (it's cheap)
    })
}

#[tracing::instrument(level = "trace", skip_all, err)]
fn write_chunk_to_module(
    storage_module: &Arc<StorageModule>,
    chunk_info: (CachedChunkIndexMetadata, CachedChunk),
    tx: &DataTransactionHeader,
    chunk_offset: TxChunkOffset,
) -> Result<(), MigrationError> {
    let data_path = Base64::from(chunk_info.1.data_path.0.clone());

    if let Some(bytes) = chunk_info.1.chunk {
        let chunk = UnpackedChunk {
            data_root: tx.data_root,
            data_size: tx.data_size,
            data_path,
            bytes,
            tx_offset: chunk_offset,
        };

        storage_module.write_data_chunk(&chunk).map_err(|e| {
            error!(
                "Failed to write chunk for data_root {:?} chunk_offset {} data_size {}: {:?}",
                tx.data_root, chunk_offset, tx.data_size, e
            );
            MigrationError::ChunkIndexWrite
        })?;
    }
    Ok(())
}

impl ChunkMigrationService {
    pub fn spawn_service(
        rx: UnboundedReceiver<Traced<ChunkMigrationServiceMessage>>,
        block_index: BlockIndex,
        storage_modules_guard: &StorageModulesReadGuard,
        db: DatabaseProvider,
        service_senders: ServiceSenders,
        config: &Config,
        runtime_handle: tokio::runtime::Handle,
    ) -> TokioServiceHandle {
        let config = config.clone();
        // let block_index = block_index.clone();
        let storage_modules_guard = storage_modules_guard.clone();
        let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();

        let handle = runtime_handle.spawn(async move {
            let data_sync_service = Self {
                shutdown: shutdown_rx,
                msg_rx: rx,
                inner: ChunkMigrationServiceInner::new(
                    block_index,
                    &storage_modules_guard,
                    db,
                    service_senders,
                    config,
                ),
            };
            data_sync_service
                .start()
                .await
                .expect("DataSync Service encountered an irrecoverable error")
        });

        TokioServiceHandle {
            name: "data_sync_service".to_string(),
            handle,
            shutdown_signal: shutdown_tx,
        }
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    async fn start(mut self) -> eyre::Result<()> {
        tracing::info!("starting DataSync Service");

        loop {
            tokio::select! {
                biased;

                _ = &mut self.shutdown => {
                    tracing::info!("Shutdown signal received for DataSync Service");
                    break;
                }

                msg = self.msg_rx.recv() => {
                    match msg {
                        Some(traced) => {
                            let (msg, _entered) = traced.into_inner();
                            self.inner.handle_message(msg)?;
                        }
                        None => {
                            tracing::warn!("Message channel closed unexpectedly");
                            break;
                        }
                    }
                }
            }
        }

        // Process remaining messages before shutdown
        while let Ok(traced) = self.msg_rx.try_recv() {
            let (msg, _entered) = traced.into_inner();
            self.inner.handle_message(msg)?;
        }

        tracing::info!("shutting down DataSync Service gracefully");
        Ok(())
    }
}
