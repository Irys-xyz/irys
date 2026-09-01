use crate::{cache_service::CacheServiceAction, services::ServiceSenders};
use irys_database::{
    block_header_by_hash, cached_chunk_by_chunk_offset,
    db::IrysDatabaseExt as _,
    db_cache::{CachedChunk, CachedChunkIndexMetadata},
    tx_header_by_txid,
};
use irys_domain::{
    BlockIndex, StorageModule, StorageModulesReadGuard, get_overlapped_storage_modules,
};
use irys_packing::unpack;
use irys_storage::{InclusiveInterval as _, ie, ii};
use irys_types::{
    Base64, BlockHash, Config, DataLedger, DataRoot, DataTransactionHeader, DataTransactionLedger,
    H256, IrysBlockHeader, LedgerChunkOffset, LedgerChunkRange, Proof, SendTraced as _,
    TokioServiceHandle, Traced, TxChunkOffset, UnpackedChunk, app_state::DatabaseProvider,
    hash_sha256, validate_path,
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
    /// Block header or tx missing from DB (legacy import / corruption / mixed restore).
    /// Callers should soft-skip the block rather than panic the service.
    #[error("Missing block or tx data for migration: {0}")]
    MissingData(String),
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
        // Soft-fail on missing/corrupt data so startup heal cannot panic-loop the node.
        let block_header = self
            .db
            .view_eyre(|tx| block_header_by_hash(tx, &block_hash, false))
            .map_err(|e| {
                MigrationError::Other(format!("db query for block {block_hash} failed: {e}"))
            })?
            .ok_or_else(|| {
                MigrationError::MissingData(format!("block header not found for {block_hash}"))
            })?;

        // For each data ledger, retrieve the tx headers and build a map
        let data_ledger_txids = block_header.get_data_ledger_tx_ids();

        let mut block_tx_map: HashMap<DataLedger, Vec<DataTransactionHeader>> = HashMap::new();
        for (ledger, tx_ids) in data_ledger_txids {
            let mut txs = Vec::new();
            for txid in tx_ids {
                let tx = self
                    .db
                    .view_eyre(|tx| tx_header_by_txid(tx, &txid))
                    .map_err(|e| {
                        MigrationError::Other(format!("db query for tx {txid} failed: {e}"))
                    })?
                    .ok_or_else(|| {
                        MigrationError::MissingData(format!(
                            "tx {txid} not found for block {block_hash}"
                        ))
                    })?;
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
        let config = self.config.clone();
        let storage_modules = Arc::new(self.storage_modules_guard.clone());
        let db = Arc::new(self.db.clone());
        let service_senders = self.service_senders.clone();

        let block_height = block.height;

        // Guard against stale BlockMigrated messages from orphaned forks.
        // After a deep reorg, recover_from_network_partition truncates the block index,
        // but previously-enqueued chunk migration messages may still arrive. Skip them
        // if the block is no longer in the canonical index.
        match block_index.get_item(block_height) {
            Some(item) if item.block_hash == block.block_hash => {}
            _ => {
                tracing::warn!(
                    block_height,
                    block_hash = %block.block_hash,
                    "skipping chunk migration for block no longer in block index (likely orphaned by reorg)"
                );
                return Ok(());
            }
        }

        // Process transactions in the order the block encodes them
        // (`block.data_ledgers: Vec<…>`), not HashMap iteration order. The
        // block producer encodes the Vec as Publish → Submit → [OneYear,
        // ThirtyDay] (see `block_producer.rs::data_ledgers`), and that order
        // is part of the consensus-signed payload — every node sees the same
        // sequence. Iterating the HashMap instead would give non-deterministic
        // ordering across nodes and runs, which doesn't affect final storage
        // state (each ledger writes into its own storage modules) but does
        // produce divergent log/trace output and divergent intermediate
        // crash-recovery state.
        for ledger_entry in block.data_ledgers.iter() {
            let Ok(ledger) = DataLedger::try_from(ledger_entry.ledger_id) else {
                tracing::warn!(
                    ledger_id = ledger_entry.ledger_id,
                    "Skipping unknown DataLedger id during chunk migration"
                );
                continue;
            };
            let Some(txs) = all_txs.get(&ledger) else {
                continue;
            };
            process_ledger_transactions(
                &block,
                ledger,
                txs,
                &block_index,
                &config,
                &storage_modules,
                &db,
            )?;
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
    config: &Config,
    storage_modules_guard: &StorageModulesReadGuard,
    db: &Arc<DatabaseProvider>,
) -> Result<(), MigrationError> {
    let path_pairs = get_tx_path_pairs(block, ledger, txs).map_err(|e| {
        MigrationError::Other(format!("tx path merklization failed for {ledger:?}: {e}"))
    })?;
    let block_offsets = get_block_offsets_in_ledger(block, ledger, block_index);
    let mut prev_chunk_offset = block_offsets.start();

    for ((_txid, tx_path), tx) in path_pairs {
        let num_chunks_in_tx: u32 = tx
            .data_size
            .div_ceil(config.consensus.chunk_size)
            .try_into()
            .map_err(|_| {
                MigrationError::Other(format!(
                    "tx {} data_size {} exceeds u32 chunk count",
                    tx.id, tx.data_size
                ))
            })?;

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
            config,
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
    config: &Config,
) -> Result<(), MigrationError> {
    for tx_chunk_offset in 0..num_chunks_in_tx {
        let tx_chunk_offset = TxChunkOffset::from(tx_chunk_offset);
        // Find which storage module intersects this chunk
        let ledger_offset = tx_chunk_range.start() + *tx_chunk_offset;
        let Some(storage_module) =
            find_storage_module(storage_modules_guard, ledger, ledger_offset.into())
        else {
            continue;
        };

        // Idempotent replay: an already-durable target needs no source body, so
        // a re-migrated block does not have to re-read or rewrite the chunk.
        if let Some(target_offsets) = storage_module
            .partition_offsets_for_data_root_chunk(tx.data_root, tx_chunk_offset)
            .map_err(|error| {
                MigrationError::Other(format!("resolving migration target: {error}"))
            })?
            && !target_offsets.is_empty()
            && target_offsets
                .iter()
                .all(|offset| storage_module.is_data_chunk_durable_at(*offset))
        {
            continue;
        }

        let Some(chunk) = load_chunk_for_migration(
            storage_modules_guard,
            db,
            ledger,
            tx,
            tx_chunk_offset,
            config,
        )?
        else {
            tracing::warn!(
                target_ledger = ?ledger,
                data_root = %tx.data_root,
                %tx_chunk_offset,
                "Chunk body unavailable during migration; leaving the target offset for data sync"
            );
            continue;
        };

        write_chunk_to_module(&storage_module, &chunk)?;
    }
    Ok(())
}

fn load_chunk_for_migration(
    storage_modules_guard: &StorageModulesReadGuard,
    db: &DatabaseProvider,
    target_ledger: DataLedger,
    tx: &DataTransactionHeader,
    tx_offset: TxChunkOffset,
    config: &Config,
) -> Result<Option<UnpackedChunk>, MigrationError> {
    let chunk_size = usize::try_from(config.consensus.chunk_size).map_err(|_| {
        MigrationError::Other(format!(
            "configured chunk size {} does not fit usize",
            config.consensus.chunk_size
        ))
    })?;
    match get_cached_chunk(db, tx.data_root, tx_offset) {
        Ok(Some((_metadata, cached))) if cached.chunk.is_some() => {
            match validate_chunk_for_migration(cached, tx, tx_offset, chunk_size) {
                Ok(chunk) => return Ok(Some(chunk)),
                Err(error) => {
                    tracing::warn!(
                        data_root = %tx.data_root,
                        %tx_offset,
                        ?error,
                        "Cached chunk failed migration validation; checking durable fallback"
                    );
                }
            }
        }
        Ok(_) => {}
        Err(error) => {
            tracing::warn!(
                data_root = %tx.data_root,
                %tx_offset,
                ?error,
                "Failed to read cached chunk during migration; checking durable fallback"
            );
        }
    }

    // Submit is the only ledger whose transaction is later copied into a
    // second ledger. Its cached body may be reclaimed after the Submit fsync,
    // so Publish migration sources that normal cache-miss path from the durable
    // Submit replica. Term-ledger transactions are written only once and do not
    // need a cross-ledger fallback. If the Submit replica was reassigned or
    // reset, the caller leaves a visible hole for data sync instead.
    if target_ledger != DataLedger::Publish {
        return Ok(None);
    }

    let storage_modules = storage_modules_guard.read().clone();
    for module in &storage_modules {
        let is_submit = module
            .partition_assignment()
            .and_then(|assignment| assignment.ledger_id)
            == Some(DataLedger::Submit as u32);
        if !is_submit {
            continue;
        }
        let Some(partition_offsets) = module
            .partition_offsets_for_data_root_chunk(tx.data_root, tx_offset)
            .map_err(|error| {
                MigrationError::Other(format!("resolving Submit fallback: {error}"))
            })?
        else {
            continue;
        };
        for partition_offset in partition_offsets {
            if !module.is_data_chunk_durable_at(partition_offset) {
                continue;
            }
            let Some(packed) = module
                .generate_full_chunk(partition_offset)
                .map_err(|error| {
                    MigrationError::Other(format!("reading durable Submit fallback: {error}"))
                })?
            else {
                continue;
            };
            let unpacked = unpack(
                &packed,
                config.consensus.entropy_packing_iterations,
                chunk_size,
                config.consensus.chain_id,
            );
            if unpacked.data_root != tx.data_root || unpacked.tx_offset != tx_offset {
                tracing::warn!(
                    data_root = %tx.data_root,
                    %tx_offset,
                    storage_module.id = module.id,
                    partition.offset = %partition_offset,
                    "Durable Submit chunk identity mismatch; checking another replica"
                );
                continue;
            }
            match validate_chunk_parts_for_migration(
                unpacked.data_path,
                unpacked.bytes,
                tx,
                tx_offset,
                chunk_size,
            ) {
                Ok(chunk) => return Ok(Some(chunk)),
                Err(error) => {
                    tracing::warn!(
                        data_root = %tx.data_root,
                        %tx_offset,
                        storage_module.id = module.id,
                        partition.offset = %partition_offset,
                        ?error,
                        "Durable Submit chunk failed migration validation; checking another replica"
                    );
                }
            }
        }
    }
    Ok(None)
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
        // The previous block's `total_chunks` is used directly as this block's
        // start offset (the count of chunks already in the ledger is the next
        // 0-indexed offset).
        // The previous block may legitimately lack this ledger entirely — e.g.
        // a Cascade term ledger (OneYear/ThirtyDay) whose first block sits right
        // after the prior block's epoch predates activation. Treat a missing
        // entry as zero chunks (the ledger started at this block) instead of
        // indexing into it, which would panic.
        block_index.get_item(block.height - 1).map_or(0, |prev| {
            prev.ledgers
                .iter()
                .find(|item| item.ledger == ledger)
                .map_or(0, |item| item.total_chunks)
        })
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
    chunk: &UnpackedChunk,
) -> Result<(), MigrationError> {
    storage_module.write_data_chunk(chunk).map_err(|e| {
        error!(
            "Failed to write chunk for data_root {:?} chunk_offset {} data_size {}: {:?}",
            chunk.data_root, chunk.tx_offset, chunk.data_size, e
        );
        MigrationError::ChunkDataWrite
    })
}

fn validate_chunk_for_migration(
    cached: CachedChunk,
    tx: &DataTransactionHeader,
    chunk_offset: TxChunkOffset,
    chunk_size: usize,
) -> Result<UnpackedChunk, MigrationError> {
    let bytes = cached.chunk.ok_or_else(|| {
        MigrationError::Other(format!(
            "cached chunk body missing for {} offset {}",
            tx.data_root, chunk_offset
        ))
    })?;
    validate_chunk_parts_for_migration(cached.data_path, bytes, tx, chunk_offset, chunk_size)
}

fn validate_chunk_parts_for_migration(
    data_path: Base64,
    bytes: Base64,
    tx: &DataTransactionHeader,
    chunk_offset: TxChunkOffset,
    chunk_size: usize,
) -> Result<UnpackedChunk, MigrationError> {
    let chunk_size_u64 = u64::try_from(chunk_size)
        .map_err(|_| MigrationError::Other("configured chunk size exceeds u64".to_string()))?;
    let min_byte_range = u64::from(*chunk_offset)
        .checked_mul(chunk_size_u64)
        .ok_or_else(|| MigrationError::Other("chunk byte range overflow".to_string()))?;
    let max_byte_range = min_byte_range
        .checked_add(chunk_size_u64)
        .map_or(tx.data_size, |end| end.min(tx.data_size));
    let target_byte_position = max_byte_range.checked_sub(1).ok_or_else(|| {
        MigrationError::Other(format!(
            "chunk has an empty byte range for {} offset {}",
            tx.data_root, chunk_offset
        ))
    })?;
    let validation = validate_path(tx.data_root.0, &data_path, u128::from(target_byte_position))
        .map_err(|error| {
            MigrationError::Other(format!(
                "data path failed revalidation for {} offset {}: {error}",
                tx.data_root, chunk_offset
            ))
        })?;
    if validation.min_byte_range != u128::from(min_byte_range)
        || validation.max_byte_range != u128::from(max_byte_range)
    {
        return Err(MigrationError::Other(format!(
            "chunk byte range mismatch for {} offset {}: expected {}..{}, got {}..{}",
            tx.data_root,
            chunk_offset,
            min_byte_range,
            max_byte_range,
            validation.min_byte_range,
            validation.max_byte_range
        )));
    }
    let expected_len = max_byte_range.saturating_sub(min_byte_range);
    if u64::try_from(bytes.len()).ok() != Some(expected_len)
        || validation.leaf_hash != hash_sha256(bytes.as_slice())
    {
        return Err(MigrationError::Other(format!(
            "chunk body failed revalidation for {} offset {}",
            tx.data_root, chunk_offset
        )));
    }
    Ok(UnpackedChunk {
        data_root: tx.data_root,
        data_size: tx.data_size,
        data_path,
        bytes,
        tx_offset: chunk_offset,
    })
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

    #[tracing::instrument(name = "chunk_migration_service_start", level = "trace", skip_all, err)]
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
