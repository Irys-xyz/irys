use crate::{cache_service::CacheServiceAction, services::ServiceSenders};
use irys_database::{
    block_header_by_hash, cached_chunk_by_chunk_offset,
    db::IrysDatabaseExt as _,
    db_cache::{CachedChunk, CachedChunkIndexMetadata},
    tx_header_by_txid,
};
use irys_domain::{
    BlockIndex, StorageModule, StorageModulesReadGuard, WriteDataChunkError,
    get_overlapped_storage_modules,
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
use tokio::sync::{Notify, mpsc::UnboundedReceiver, oneshot};
use tracing::{error, instrument};

mod body_worker;

pub struct ChunkMigrationService {
    shutdown: Shutdown,
    msg_rx: UnboundedReceiver<Traced<ChunkMigrationServiceMessage>>,
    inner: ChunkMigrationServiceInner,
}

/// Moves a migrated block's data into the storage modules, in two decoupled
/// halves:
///
/// - **Index path** (this service, in block order): for every ledger tx,
///   writes the `tx_path` / `data_root` mappings into each overlapping
///   submodule's index and, in the same transaction, a
///   `PendingBodyMigrationsByOffset` row recording that the tx's chunk bodies
///   are still owed. MDBX only — never touches chunk data — so one huge tx can
///   never delay the next block's indexes.
/// - **Body path** (`body_worker`, background): drains those rows at disk
///   speed, sourcing bodies from the chunk cache or the durable Submit replica,
///   under a per-pass budget and a pending-write byte ceiling. Whatever it
///   cannot source locally is left as Entropy for data sync.
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
    /// Wakes the body-migration worker once a block's indexes have committed.
    pub body_wakeup: Arc<Notify>,
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
        body_wakeup: Arc<Notify>,
    ) -> Self {
        tracing::info!("service started: chunk_migration");
        Self {
            block_index,
            config,
            storage_modules_guard: storage_modules_guard.clone(),
            db,
            service_senders,
            body_wakeup,
        }
    }

    /// Infallible by design: one block whose migration fails must not take the
    /// service — and every later block's indexes — down with it. The failure is
    /// logged; index heal re-migrates blocks whose indexes are missing.
    #[tracing::instrument(level = "trace", skip_all)]
    pub fn handle_message(&mut self, msg: ChunkMigrationServiceMessage) {
        match msg {
            ChunkMigrationServiceMessage::BlockMigrated(block_header, all_txs) => {
                if let Err(error) = self.on_block_migrated(block_header.clone(), all_txs) {
                    tracing::error!(
                        block.height = block_header.height,
                        block.hash = %block_header.block_hash,
                        ?error,
                        "chunk migration failed for block; continuing with later blocks"
                    );
                }
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
            )?;
        }

        // This block's indexes are committed; the bodies they describe are now
        // owed as `PendingBodyMigrationsByOffset` rows. Wake the worker — it
        // also polls, so a lost wake-up only costs latency, never progress.
        self.body_wakeup.notify_one();

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

/// Indexes one ledger's transactions for a migrated block. **Index only**: the
/// `tx_path` / `data_root` mappings commit here, in block order, and the chunk
/// bodies they describe are left as `PendingBodyMigrationsByOffset` rows for the
/// body worker (`body_worker.rs`). Nothing in this path reads or writes chunk
/// data, so one huge tx can never delay the next block's indexes.
#[tracing::instrument(level = "trace", skip_all, err)]
pub fn process_ledger_transactions(
    block: &Arc<IrysBlockHeader>,
    ledger: DataLedger,
    txs: &[DataTransactionHeader],
    block_index: &BlockIndex,
    config: &Config,
    storage_modules_guard: &StorageModulesReadGuard,
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
            block.height,
        )?;

        prev_chunk_offset += num_chunks_in_tx as u64;
    }

    Ok(())
}

/// Source the body for `tx_offset` of `data_root`: the chunk cache first, then
/// (Publish only) the durable Submit replica. `None` means data sync's problem.
fn load_chunk_for_migration(
    storage_modules_guard: &StorageModulesReadGuard,
    db: &DatabaseProvider,
    target_ledger: DataLedger,
    data_root: DataRoot,
    data_size: u64,
    tx_offset: TxChunkOffset,
    config: &Config,
) -> Result<Option<UnpackedChunk>, MigrationError> {
    let chunk_size = usize::try_from(config.consensus.chunk_size).map_err(|_| {
        MigrationError::Other(format!(
            "configured chunk size {} does not fit usize",
            config.consensus.chunk_size
        ))
    })?;
    match get_cached_chunk(db, data_root, tx_offset) {
        Ok(Some((_metadata, cached))) if cached.chunk.is_some() => {
            match validate_chunk_for_migration(cached, data_root, data_size, tx_offset, chunk_size)
            {
                Ok(chunk) => return Ok(Some(chunk)),
                Err(error) => {
                    tracing::warn!(
                        data_root = %data_root,
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
                data_root = %data_root,
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
            .partition_offsets_for_data_root_chunk(data_root, tx_offset)
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
            if unpacked.data_root != data_root || unpacked.tx_offset != tx_offset {
                tracing::warn!(
                    data_root = %data_root,
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
                data_root,
                data_size,
                tx_offset,
                chunk_size,
            ) {
                Ok(chunk) => return Ok(Some(chunk)),
                Err(error) => {
                    tracing::warn!(
                        data_root = %data_root,
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
    block_height: u64,
) -> Result<(), MigrationError> {
    let overlapped_modules =
        get_overlapped_storage_modules(storage_modules_guard, ledger, &tx_chunk_range);

    for storage_module in overlapped_modules {
        storage_module
            .index_transaction_data(
                data_tx,
                &tx_path_proof.to_vec(),
                tx_chunk_range,
                block_height,
            )
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

#[tracing::instrument(level = "trace", skip_all, err)]
fn write_chunk_to_module(
    storage_module: &Arc<StorageModule>,
    chunk: &UnpackedChunk,
) -> Result<(), MigrationError> {
    storage_module.write_data_chunk(chunk).map_err(|e| match e {
        // Recovery holds the module's data writes; the worker defers, so this
        // is expected and not an error worth an error-level log.
        WriteDataChunkError::WritesPaused => {
            MigrationError::Other("storage module data writes paused for recovery".to_owned())
        }
        e => {
            error!(
                "Failed to write chunk for data_root {:?} chunk_offset {} data_size {}: {:?}",
                chunk.data_root, chunk.tx_offset, chunk.data_size, e
            );
            MigrationError::ChunkDataWrite
        }
    })
}

fn validate_chunk_for_migration(
    cached: CachedChunk,
    data_root: DataRoot,
    data_size: u64,
    chunk_offset: TxChunkOffset,
    chunk_size: usize,
) -> Result<UnpackedChunk, MigrationError> {
    let bytes = cached.chunk.ok_or_else(|| {
        MigrationError::Other(format!(
            "cached chunk body missing for {} offset {}",
            data_root, chunk_offset
        ))
    })?;
    validate_chunk_parts_for_migration(
        cached.data_path,
        bytes,
        data_root,
        data_size,
        chunk_offset,
        chunk_size,
    )
}

fn validate_chunk_parts_for_migration(
    data_path: Base64,
    bytes: Base64,
    data_root: DataRoot,
    data_size: u64,
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
        .map_or(data_size, |end| end.min(data_size));
    let target_byte_position = max_byte_range.checked_sub(1).ok_or_else(|| {
        MigrationError::Other(format!(
            "chunk has an empty byte range for {} offset {}",
            data_root, chunk_offset
        ))
    })?;
    let validation = validate_path(data_root.0, &data_path, u128::from(target_byte_position))
        .map_err(|error| {
            MigrationError::Other(format!(
                "data path failed revalidation for {} offset {}: {error}",
                data_root, chunk_offset
            ))
        })?;
    if validation.min_byte_range != u128::from(min_byte_range)
        || validation.max_byte_range != u128::from(max_byte_range)
    {
        return Err(MigrationError::Other(format!(
            "chunk byte range mismatch for {} offset {}: expected {}..{}, got {}..{}",
            data_root,
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
            data_root, chunk_offset
        )));
    }
    Ok(UnpackedChunk {
        data_root,
        data_size,
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

        // Chunk bodies are written by a separate worker so a slow or busy disk
        // can never hold up the next block's indexes (see `body_worker.rs`).
        let body_wakeup = Arc::new(Notify::new());
        runtime_handle.spawn(
            body_worker::BodyMigrationWorker::new(
                storage_modules_guard.clone(),
                db.clone(),
                config.clone(),
                body_wakeup.clone(),
                shutdown_rx.clone(),
            )
            .run(),
        );

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
                    body_wakeup,
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
                            self.inner.handle_message(msg);
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
            self.inner.handle_message(msg);
        }

        tracing::info!("shutting down DataSync Service gracefully");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::body_worker::{
        BodyMigrationWorker, MAX_BODY_MIGRATION_ATTEMPTS, PENDING_WRITE_CEILING_BATCHES,
        PENDING_WRITE_CEILING_MIN_CHUNKS, PassStats, pending_write_ceiling_bytes,
    };
    use super::*;
    use irys_database::{
        IrysDatabaseArgs as _, cache_chunk, cache_data_root, insert_block_header, insert_tx_header,
        open_or_create_db, submodule::tables::PendingBodyMigration, tables::IrysTables,
    };
    use irys_domain::{ChunkType, StorageModuleInfo};
    use irys_testing_utils::TempDirBuilder;
    use irys_types::{
        BlockIndexItem, ConsensusConfig, DataTransaction, H256List, IrysAddress, LedgerIndexItem,
        NodeConfig, PartitionChunkOffset, irys::IrysSigner, partition::PartitionAssignment,
        partition_chunk_offset_ie,
    };
    use std::{sync::RwLock, time::Duration};

    struct Fixture {
        _tmp: irys_testing_utils::utils::tempfile::TempDir,
        shutdown_signal: Option<reth::tasks::shutdown::Signal>,
        shutdown: Shutdown,
        config: Config,
        sm: Arc<StorageModule>,
        guard: StorageModulesReadGuard,
        db: DatabaseProvider,
        block_index: BlockIndex,
        block: Arc<IrysBlockHeader>,
        tx: DataTransaction,
        data: Vec<u8>,
    }

    /// One Submit-assigned module (20 chunks, slot 0) and a height-0 block
    /// carrying a single `num_chunks`-chunk Submit tx. The node DB starts with
    /// no cached bodies; `seed_cache` adds them. `max_pending_write_bytes` is
    /// the module's pending-write ceiling (`None` = derived default).
    fn fixture(num_chunks: u64, max_pending_write_bytes: Option<u64>) -> eyre::Result<Fixture> {
        let tmp = TempDirBuilder::new().with_tracing().build();
        let chunk_size = 32_u64;
        let mut node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size,
                num_chunks_in_partition: 20,
                num_chunks_in_recall_range: 2,
                num_partitions_per_slot: 1,
                entropy_packing_iterations: 1,
                chain_id: 1,
                ..ConsensusConfig::testing()
            }),
            base_directory: tmp.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        node_config.storage.max_pending_write_bytes = max_pending_write_bytes;
        let config = Config::new_with_random_peer_id(node_config);

        let sm = Arc::new(StorageModule::new(
            &StorageModuleInfo {
                id: 0,
                partition_assignment: Some(PartitionAssignment {
                    ledger_id: Some(DataLedger::Submit.into()),
                    slot_index: Some(0),
                    miner_address: IrysAddress::from([0xAA; 20]),
                    partition_hash: H256::random(),
                }),
                submodules: vec![(partition_chunk_offset_ie!(0, 20), "hdd0".into())],
            },
            &config,
        )?);
        let guard = StorageModulesReadGuard::new(Arc::new(RwLock::new(vec![sm.clone()])));

        let db = DatabaseProvider(Arc::new(open_or_create_db(
            tmp.path().join("irys_db"),
            IrysTables::ALL,
            reth_db::mdbx::DatabaseArguments::irys_testing()?,
        )?));
        let block_index = BlockIndex::new_for_testing(db.clone());

        let data = vec![7_u8; (chunk_size * num_chunks) as usize];
        let signer = IrysSigner::random_signer(&config.consensus);
        let tx = signer.sign_transaction(signer.create_transaction(data.clone(), H256::zero())?)?;
        let (tx_root, _) =
            DataTransactionLedger::merklize_tx_root(std::slice::from_ref(&tx.header));

        let mut block = IrysBlockHeader::new_mock_header();
        block.height = 0;
        {
            let submit = &mut block.data_ledgers[DataLedger::Submit];
            submit.tx_root = tx_root;
            submit.total_chunks = num_chunks;
            submit.tx_ids = H256List(vec![tx.header.id]);
        }

        let (shutdown_signal, shutdown) = reth::tasks::shutdown::signal();
        Ok(Fixture {
            _tmp: tmp,
            shutdown_signal: Some(shutdown_signal),
            shutdown,
            config,
            sm,
            guard,
            db,
            block_index,
            block: Arc::new(block),
            tx,
            data,
        })
    }

    impl Fixture {
        /// The ordered index path, exactly as `on_block_migrated` runs it.
        fn index(&self) -> Result<(), MigrationError> {
            process_ledger_transactions(
                &self.block,
                DataLedger::Submit,
                std::slice::from_ref(&self.tx.header),
                &self.block_index,
                &self.config,
                &self.guard,
            )
        }

        /// Put every chunk body of the tx into the node's chunk cache.
        fn seed_cache(&self) -> eyre::Result<()> {
            self.db.update_eyre(|wtx| {
                cache_data_root(wtx, &self.tx.header, None)?;
                for (i, node) in self.tx.chunks.iter().enumerate() {
                    let chunk = UnpackedChunk {
                        data_root: self.tx.header.data_root,
                        data_size: self.tx.header.data_size,
                        data_path: Base64(self.tx.proofs[i].proof.clone()),
                        bytes: Base64(self.data[node.min_byte_range..node.max_byte_range].to_vec()),
                        tx_offset: TxChunkOffset::from(u32::try_from(i)?),
                    };
                    cache_chunk(wtx, &chunk)?;
                }
                Ok(())
            })
        }

        /// A fresh worker over the same modules and DB — also what a restart looks like.
        fn worker(&self) -> BodyMigrationWorker {
            BodyMigrationWorker::new(
                self.guard.clone(),
                self.db.clone(),
                self.config.clone(),
                Arc::new(Notify::new()),
                self.shutdown.clone(),
            )
        }

        fn rows(&self) -> eyre::Result<Vec<(PartitionChunkOffset, PendingBodyMigration)>> {
            self.sm.pending_body_migrations()
        }

        fn chunk_type(&self, offset: u32) -> Option<ChunkType> {
            self.sm.get_chunk_type(&PartitionChunkOffset::from(offset))
        }

        /// Persist the block the way block migration would have — header and tx
        /// header in the node DB, item in the block index — so `on_block_migrated`
        /// (and heal's `UpdateStorageModuleIndexes`) can run against it.
        fn persist_block(&self) -> eyre::Result<()> {
            self.db.update_eyre(|tx| {
                insert_block_header(tx, &self.block)?;
                insert_tx_header(tx, &self.tx.header)?;
                Ok(())
            })?;
            let submit = &self.block.data_ledgers[DataLedger::Submit];
            self.block_index.push_item(
                &BlockIndexItem {
                    block_hash: self.block.block_hash,
                    num_ledgers: 1,
                    ledgers: vec![LedgerIndexItem {
                        total_chunks: submit.total_chunks,
                        tx_root: submit.tx_root,
                        ledger: DataLedger::Submit,
                    }],
                },
                self.block.height,
            )
        }

        fn service_inner(&self) -> ChunkMigrationServiceInner {
            let (service_senders, _receivers) = ServiceSenders::new();
            ChunkMigrationServiceInner::new(
                self.block_index.clone(),
                &self.guard,
                self.db.clone(),
                service_senders,
                self.config.clone(),
                Arc::new(Notify::new()),
            )
        }

        fn migrated_msg(&self, block: Arc<IrysBlockHeader>) -> ChunkMigrationServiceMessage {
            let mut txs = HashMap::new();
            txs.insert(DataLedger::Submit, vec![self.tx.header.clone()]);
            ChunkMigrationServiceMessage::BlockMigrated(block, Arc::new(txs))
        }
    }

    /// Index heal's `UpdateStorageModuleIndexes` runs the same index-only path:
    /// rows appear, no chunk data moves — so a heal pass can never re-create
    /// the body-write stall it exists to repair.
    #[test]
    fn heal_reindex_is_index_only() -> eyre::Result<()> {
        let f = fixture(3, Some(u64::MAX))?;
        f.seed_cache()?;
        f.persist_block()?;
        let mut inner = f.service_inner();

        inner.on_update_storage_module_indexes(f.block.block_hash)?;

        assert_eq!(f.rows()?.len(), 1);
        assert!(!f.sm.has_pending_writes());
        assert_ne!(f.chunk_type(0), Some(ChunkType::Data));
        assert!(
            f.sm.partition_offsets_for_data_root_chunk(
                f.tx.header.data_root,
                TxChunkOffset::from(0)
            )?
            .is_some()
        );
        Ok(())
    }

    /// A block whose migration fails is logged and skipped; the next block is
    /// still processed. Previously the error unwound `start()` and killed the
    /// service for the life of the node.
    #[test]
    fn failed_block_does_not_stop_later_blocks() -> eyre::Result<()> {
        let f = fixture(3, Some(u64::MAX))?;
        f.persist_block()?;
        let mut inner = f.service_inner();

        // Same hash (passes the block-index guard), corrupted tx_root: merklization fails.
        let mut bad = (*f.block).clone();
        bad.data_ledgers[DataLedger::Submit].tx_root = H256::random();
        inner.handle_message(f.migrated_msg(Arc::new(bad)));
        assert!(f.rows()?.is_empty(), "failed block must index nothing");

        inner.handle_message(f.migrated_msg(f.block.clone()));
        assert_eq!(f.rows()?.len(), 1, "later block still migrates");
        Ok(())
    }

    /// The ordered path commits the index and the job row and touches no chunk
    /// data — even when the bodies are sitting in the cache.
    #[test]
    fn index_path_writes_index_and_job_row_but_no_bodies() -> eyre::Result<()> {
        let f = fixture(3, Some(u64::MAX))?;
        f.seed_cache()?;
        f.index()?;

        let rows = f.rows()?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, PartitionChunkOffset::from(0));
        assert_eq!(rows[0].1.data_root, f.tx.header.data_root);
        assert_eq!(rows[0].1.attempts, 0);

        assert!(!f.sm.has_pending_writes());
        assert_ne!(f.chunk_type(0), Some(ChunkType::Data));
        assert_eq!(
            f.sm.partition_offsets_for_data_root_chunk(
                f.tx.header.data_root,
                TxChunkOffset::from(2)
            )?,
            Some(vec![PartitionChunkOffset::from(2)])
        );
        Ok(())
    }

    /// The worker sources bodies from the chunk cache and settles the row only
    /// once the storage module reports them durable (after the flush), so a
    /// crash between write and fsync cannot lose the job.
    #[tokio::test]
    async fn worker_writes_bodies_from_cache_and_settles_after_flush() -> eyre::Result<()> {
        let f = fixture(3, Some(u64::MAX))?;
        f.sm.pack_with_zeros();
        f.seed_cache()?;
        f.index()?;
        let worker = f.worker();

        let pass = worker.drain_pass().await;
        assert_eq!((pass.rows, pass.written, pass.settled), (1, 3, 0));
        for offset in 0..3 {
            assert!(f.sm.is_data_write_pending_at(PartitionChunkOffset::from(offset)));
        }
        assert_eq!(f.rows()?.len(), 1, "not durable yet: the row must survive");

        // Before the flush everything is in flight: nothing is re-read or re-written.
        let pass = worker.drain_pass().await;
        assert_eq!((pass.written, pass.settled), (0, 0));
        assert_eq!(f.rows()?.len(), 1);

        f.sm.force_sync_pending_chunks()?;
        let pass = worker.drain_pass().await;
        assert_eq!(
            (pass.rows, pass.written, pass.settled, pass.retired),
            (1, 0, 1, 0)
        );
        assert!(f.rows()?.is_empty());
        for offset in 0..3 {
            assert_eq!(f.chunk_type(offset), Some(ChunkType::Data));
        }

        // Idle pass is a no-op.
        assert_eq!(worker.drain_pass().await, PassStats::default());
        Ok(())
    }

    /// With no local source (cold cache, no Submit replica) every pass stalls;
    /// after `MAX_BODY_MIGRATION_ATTEMPTS` the row is retired and the offsets
    /// stay Entropy holes for data sync. The index is untouched throughout.
    #[tokio::test]
    async fn worker_retires_job_with_no_local_source() -> eyre::Result<()> {
        let f = fixture(3, Some(u64::MAX))?;
        f.sm.pack_with_zeros();
        f.index()?;
        let worker = f.worker();

        for attempt in 1..MAX_BODY_MIGRATION_ATTEMPTS {
            let pass = worker.drain_pass().await;
            assert_eq!((pass.written, pass.settled, pass.retired), (0, 0, 0));
            assert_eq!(f.rows()?[0].1.attempts, attempt);
        }
        let pass = worker.drain_pass().await;
        assert_eq!(pass.retired, 1);
        assert!(f.rows()?.is_empty());

        assert_ne!(f.chunk_type(0), Some(ChunkType::Data));
        assert!(
            f.sm.partition_offsets_for_data_root_chunk(
                f.tx.header.data_root,
                TxChunkOffset::from(0)
            )?
            .is_some()
        );
        Ok(())
    }

    /// The per-pass write budget paces, it does not abandon: the row survives a
    /// partial pass and a *fresh* worker (a restart) continues from the first
    /// non-durable offset.
    #[tokio::test]
    async fn worker_budget_paces_writes_across_passes_and_restarts() -> eyre::Result<()> {
        let f = fixture(3, Some(u64::MAX))?;
        f.sm.pack_with_zeros();
        f.seed_cache()?;
        f.index()?;

        let pass = f.worker().with_writes_per_pass(2).drain_pass().await;
        assert_eq!((pass.written, pass.settled), (2, 0));
        assert!(f.sm.is_data_write_pending_at(PartitionChunkOffset::from(1)));
        assert!(!f.sm.is_data_write_pending_at(PartitionChunkOffset::from(2)));
        assert_eq!(
            f.rows()?[0].1.attempts,
            0,
            "a budgeted pass is progress, not a stall"
        );

        let pass = f.worker().with_writes_per_pass(2).drain_pass().await;
        assert_eq!((pass.written, pass.settled), (1, 0));

        f.sm.force_sync_pending_chunks()?;
        assert_eq!(f.worker().drain_pass().await.settled, 1);
        assert!(f.rows()?.is_empty());
        Ok(())
    }

    /// The pending-write ceiling is backpressure, not a budget: once the module
    /// holds `ceiling` bytes awaiting flush the pass stops writing, the row
    /// survives with no attempt penalty, and writing resumes only after the
    /// storage service has flushed. Here nothing flushes until we force it.
    #[tokio::test]
    async fn worker_pauses_at_pending_write_ceiling_until_flushed() -> eyre::Result<()> {
        let chunk_size = 32_u64;
        let f = fixture(3, Some(2 * chunk_size))?;
        f.sm.pack_with_zeros();
        f.sm.force_sync_pending_chunks()?;
        f.seed_cache()?;
        f.index()?;
        let worker = f.worker();

        let pass = worker.drain_pass().await;
        assert_eq!((pass.written, pass.throttled, pass.settled), (2, 1, 0));
        assert_eq!(f.sm.pending_write_bytes(), 2 * chunk_size);
        assert_eq!(f.rows()?[0].1.attempts, 0, "backpressure is not a stall");

        // Still nothing flushed: the worker must not pile more into memory.
        let pass = worker.drain_pass().await;
        assert_eq!((pass.written, pass.throttled), (0, 1));
        assert_eq!(f.sm.pending_write_bytes(), 2 * chunk_size);

        f.sm.force_sync_pending_chunks()?;
        assert_eq!(f.sm.pending_write_bytes(), 0);
        let pass = worker.drain_pass().await;
        assert_eq!((pass.written, pass.throttled), (1, 0));

        f.sm.force_sync_pending_chunks()?;
        assert_eq!(worker.drain_pass().await.settled, 1);
        assert!(f.rows()?.is_empty());
        Ok(())
    }

    /// Without an explicit ceiling the worker allows two flush batches per
    /// submodule in memory (one being flushed, one being filled), floored so a
    /// tiny `num_writes_before_sync` cannot starve it.
    #[test]
    fn pending_write_ceiling_defaults_to_two_flush_batches_per_submodule() -> eyre::Result<()> {
        let f = fixture(1, None)?;
        let nwbs = f.config.node_config.storage.num_writes_before_sync;
        assert_eq!(
            nwbs, 1,
            "testing config: the floor must be what applies here"
        );
        let expected = (PENDING_WRITE_CEILING_BATCHES * nwbs).max(PENDING_WRITE_CEILING_MIN_CHUNKS)
            * f.config.consensus.chunk_size
            * f.sm.submodule_count() as u64;
        assert_eq!(pending_write_ceiling_bytes(&f.config, &f.sm), expected);
        assert_eq!(
            expected,
            PENDING_WRITE_CEILING_MIN_CHUNKS * f.config.consensus.chunk_size,
            "with num_writes_before_sync = 1 the floor is the ceiling"
        );

        let g = fixture(1, Some(12_345))?;
        assert_eq!(pending_write_ceiling_bytes(&g.config, &g.sm), 12_345);

        // A configured ceiling below one flush batch is raised to it: under that
        // level the threshold flush never fires and migration would crawl.
        let h = fixture(1, Some(1))?;
        let one_batch = h.config.node_config.storage.num_writes_before_sync
            * h.config.consensus.chunk_size
            * h.sm.submodule_count() as u64;
        assert_eq!(pending_write_ceiling_bytes(&h.config, &h.sm), one_batch);
        Ok(())
    }

    /// Offsets without entropy cannot take a body (`write_data_chunk` writes
    /// nothing there). The worker must not count those as written — that would
    /// spin the drain loop and re-read the cache every pass for as long as
    /// packing takes — but wait, penalty-free, until entropy lands.
    #[tokio::test]
    async fn worker_waits_for_entropy_instead_of_spinning() -> eyre::Result<()> {
        let f = fixture(3, Some(u64::MAX))?;
        // Deliberately not packed: every offset is Uninitialized.
        f.seed_cache()?;
        f.index()?;
        let worker = f.worker();

        for _ in 0..2 {
            let pass = worker.drain_pass().await;
            assert_eq!((pass.written, pass.unwritable, pass.throttled), (0, 3, 0));
            assert!(
                !f.sm.has_pending_writes(),
                "nothing may be queued without entropy"
            );
            assert_eq!(
                f.rows()?[0].1.attempts,
                0,
                "waiting on packing is not a stall"
            );
        }

        f.sm.pack_with_zeros();
        f.sm.force_sync_pending_chunks()?;
        let pass = worker.drain_pass().await;
        assert_eq!((pass.written, pass.unwritable), (3, 0));
        Ok(())
    }

    /// While recovery holds a module's data writes, the worker defers its jobs
    /// untouched — nothing queued, no attempt counted — and resumes afterwards.
    #[tokio::test]
    async fn worker_defers_while_module_writes_are_paused() -> eyre::Result<()> {
        let f = fixture(3, Some(u64::MAX))?;
        f.sm.pack_with_zeros();
        f.seed_cache()?;
        f.index()?;
        let worker = f.worker();

        f.sm.pause_data_writes();
        let pass = worker.drain_pass().await;
        assert_eq!((pass.written, pass.settled, pass.retired), (0, 0, 0));
        assert!(!f.sm.has_pending_writes());
        assert_eq!(f.rows()?[0].1.attempts, 0, "a paused module is not a stall");

        f.sm.resume_data_writes();
        assert_eq!(worker.drain_pass().await.written, 3);
        Ok(())
    }

    /// After the bodies are durable, pausing still must not settle. Recovery
    /// may be about to remap those offsets; the next pass after resume settles
    /// if they are still Data.
    #[tokio::test]
    async fn worker_does_not_settle_a_durable_job_while_paused() -> eyre::Result<()> {
        let f = fixture(3, Some(u64::MAX))?;
        f.sm.pack_with_zeros();
        f.seed_cache()?;
        f.index()?;
        let worker = f.worker();

        assert_eq!(worker.drain_pass().await.written, 3);
        f.sm.force_sync_pending_chunks()?;

        f.sm.pause_data_writes();
        let pass = worker.drain_pass().await;
        assert_eq!((pass.written, pass.settled, pass.retired), (0, 0, 0));
        assert_eq!(f.rows()?.len(), 1);

        f.sm.resume_data_writes();
        assert_eq!(worker.drain_pass().await.settled, 1);
        assert!(f.rows()?.is_empty());
        Ok(())
    }

    /// A job whose index was cleared under it fails on every offset
    /// (`DataRootNotFound`). Failures count as no-progress passes, so the row
    /// is retired after the limit instead of erroring every tick forever.
    #[tokio::test]
    async fn worker_retires_job_that_keeps_failing() -> eyre::Result<()> {
        let f = fixture(3, Some(u64::MAX))?;
        f.sm.pack_with_zeros();
        f.seed_cache()?;
        f.index()?;
        f.sm.clear_data_root_infos_in_range(
            PartitionChunkOffset::from(0),
            PartitionChunkOffset::from(2),
            &[f.tx.header.data_root],
        )?;
        let worker = f.worker();

        for attempt in 1..MAX_BODY_MIGRATION_ATTEMPTS {
            let pass = worker.drain_pass().await;
            assert_eq!((pass.written, pass.retired), (0, 0));
            assert_eq!(f.rows()?[0].1.attempts, attempt);
        }
        assert_eq!(worker.drain_pass().await.retired, 1);
        assert!(f.rows()?.is_empty());
        Ok(())
    }

    /// `run()` end to end: drains on startup with no wake-up, resumes once the
    /// storage service (here: us) flushes, and exits on shutdown.
    #[tokio::test]
    async fn run_loop_drains_and_exits_on_shutdown() -> eyre::Result<()> {
        let mut f = fixture(3, Some(u64::MAX))?;
        f.sm.pack_with_zeros();
        f.seed_cache()?;
        f.index()?;
        let handle = tokio::spawn(f.worker().run());

        // Stand in for the storage-module service's 1s flush tick.
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                f.sm.force_sync_pending_chunks().expect("flush");
                if f.rows().expect("rows").is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("worker never settled the job");
        for offset in 0..3 {
            assert_eq!(f.chunk_type(offset), Some(ChunkType::Data));
        }

        f.shutdown_signal.take().expect("signal").fire();
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("worker did not stop on shutdown")?;
        Ok(())
    }
}
