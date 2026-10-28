use crate::{cache_service::CacheServiceAction, services::ServiceSenders};
use irys_database::{
    block_header_by_hash, cached_chunk_by_chunk_offset,
    db::IrysDatabaseExt as _,
    db_cache::{CachedChunk, CachedChunkIndexMetadata},
    tx_header_by_txid,
};
use irys_domain::{
    BatchEnqueueItem, BlockIndex, StorageModule, StorageModulesReadGuard, WriteDataChunkError,
    get_overlapped_storage_modules,
};
use irys_storage::ie;
use irys_types::DataTreeRoot;
use irys_types::{
    Base64, BlockHash, Config, DataCommitment, DataLedger, DataTransactionHeader,
    DataTransactionLedger, H256, IrysBlockHeader, LedgerChunkOffset, LedgerChunkRange, Proof,
    SendTraced as _, TokioServiceHandle, Traced, TxChunkOffset, UnpackedChunk,
    app_state::DatabaseProvider,
    data_tree::{Geometry, data_commitment, verify_data_chunk},
};
use reth::tasks::shutdown::Shutdown;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tokio::sync::{Notify, mpsc::UnboundedReceiver, oneshot};
use tracing::{Instrument as _, error, instrument};

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
    /// Wakes the body-migration worker once a block's indexes are durable.
    pub body_wakeup: Arc<Notify>,
    /// Test-only. Runs on the WAL-sync thread after that sync and before side effects.
    #[cfg(test)]
    index_durable_hook: Option<IndexDurableHook>,
}

/// Observes the moment a block's index registrations are durable.
/// Called on the thread that performed the sync.
#[cfg(test)]
#[derive(Clone)]
struct IndexDurableHook(Arc<dyn Fn() + Send + Sync>);

#[cfg(test)]
impl std::fmt::Debug for IndexDurableHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("IndexDurableHook(..)")
    }
}

#[cfg(test)]
impl IndexDurableHook {
    fn new(hook: impl Fn() + Send + Sync + 'static) -> Self {
        Self(Arc::new(hook))
    }

    fn call(&self) {
        (self.0)();
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MigrationError {
    /// Recovery holds this module's data writes. The body worker defers; this
    /// is not a job failure.
    #[error("storage module data writes paused for recovery")]
    WritesPaused,
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
    #[error("Chunk migration error: {0}")]
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
            #[cfg(test)]
            index_durable_hook: None,
        }
    }

    #[cfg(test)]
    fn set_index_durable_hook(&mut self, hook: impl Fn() + Send + Sync + 'static) {
        self.index_durable_hook = Some(IndexDurableHook::new(hook));
    }

    /// Infallible by design: one block whose migration fails must not take the
    /// service — and every later block's indexes — down with it. The failure is
    /// logged; index heal re-migrates blocks whose indexes are missing.
    #[tracing::instrument(level = "trace", skip_all)]
    pub async fn handle_message(&mut self, msg: ChunkMigrationServiceMessage) {
        match msg {
            ChunkMigrationServiceMessage::BlockMigrated(block_header, all_txs) => {
                if let Err(error) = self.on_block_migrated(block_header.clone(), all_txs).await {
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
                let response_value = self.on_update_storage_module_indexes(block_hash).await;
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

    async fn on_update_storage_module_indexes(
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

        // Index only. Chunk bytes stay pending rows for the body worker.
        self.on_block_migrated(Arc::new(block_header), Arc::new(block_tx_map))
            .await?;

        Ok(())
    }

    #[instrument(skip_all, fields(
        height = %block_header.height,
        hash = %block_header.block_hash
    ))]

    async fn on_block_migrated(
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

        // Follow `block.data_ledgers` order. HashMap iteration would make the
        // crash-recovery intermediate state depend on hash order. Final
        // storage does not: each ledger writes its own modules.
        let mut touched = Vec::new();
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
                &mut touched,
            )?;
        }

        // One SyncWAL per submodule this block wrote. Per-tx registrations
        // stay unsynced. The wake and `OnBlockMigrated` below treat the rows
        // as durable, so a failed sync must not publish them.
        // RocksDB's WAL flush blocks. Run it on the blocking pool so this
        // worker can run other tasks, and publish only after it returns.
        #[cfg(test)]
        let index_durable_hook = self.index_durable_hook.clone();
        // The flush runs on another thread, so re-enter the span there.
        let sync_span = tracing::Span::current();
        let synced = tokio::task::spawn_blocking(move || {
            let _entered = sync_span.entered();
            let result = Self::sync_block_indexes(&touched);
            #[cfg(test)]
            if result.is_ok()
                && let Some(hook) = &index_durable_hook
            {
                hook.call();
            }
            result
        })
        .await;
        match synced {
            Ok(result) => result?,
            Err(err) if err.is_panic() => std::panic::resume_unwind(err.into_panic()),
            Err(err) => {
                return Err(MigrationError::Other(format!(
                    "index sync task ended: {err}"
                )));
            }
        }

        // The bodies are now owed as `PendingBodyMigrationsByOffset` rows.
        // Wake the worker — it also polls, so a lost wake-up only costs
        // latency, never progress.
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

    /// One synced WAL write per dirty submodule of each storage module this
    /// block indexed. A module touched by several txs is synced once.
    ///
    /// `sync_group` is a no-op on a clean submodule and on an MDBX index
    /// whose group commit is off. A failure returns [`MigrationError::ChunkIndexWrite`].
    fn sync_block_indexes(touched: &[Arc<StorageModule>]) -> Result<(), MigrationError> {
        let mut seen = HashSet::new();
        for module in touched {
            if !seen.insert(module.id) {
                continue;
            }
            if let Err(err) = module.sync_index_registrations() {
                error!(
                    storage_module.id = module.id,
                    %err,
                    "failed to sync block index registrations"
                );
                return Err(MigrationError::ChunkIndexWrite);
            }
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
    touched: &mut Vec<Arc<StorageModule>>,
) -> Result<(), MigrationError> {
    let path_pairs = get_tx_path_pairs(block, ledger, txs).map_err(|e| {
        MigrationError::Other(format!("tx path merklization failed for {ledger:?}: {e}"))
    })?;
    let mut prev_chunk_offset = ledger_start_offset(block, ledger, block_index)?;

    for (tx_index, ((_txid, tx_path), tx)) in path_pairs.into_iter().enumerate() {
        let tx_index = u32::try_from(tx_index)
            .map_err(|_| MigrationError::Other(format!("tx index {tx_index} does not fit u32")))?;
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

        // Recovery holds the exclusive gate from its first clear through the
        // block-index truncate. Take the shared gates before the re-check so
        // the truncate cannot land between the check and this commit.
        let mut modules: Vec<Arc<StorageModule>> =
            storage_modules_guard.read().iter().cloned().collect();
        modules.sort_by_key(|module| module.id);
        let _index_recovery: Vec<_> = modules
            .iter()
            .map(|module| module.lock_index_recovery_shared())
            .collect();
        // The test pause is after the lock returns and before the hash read.
        // A wait that ends while this flag is clear has not reached the gate.
        #[cfg(test)]
        recovery_lock_pause::pause_after_shared_lock();
        let still_canonical = matches!(
            block_index.get_item(block.height),
            Some(item) if item.block_hash == block.block_hash
        );
        if !still_canonical {
            tracing::warn!(
                block_height = block.height,
                block_hash = %block.block_hash,
                "skipping chunk migration for block no longer in block index (likely orphaned by reorg)"
            );
            return Ok(());
        }

        update_storage_module_indexes(
            tx,
            &tx_path.proof,
            tx_chunk_range,
            ledger,
            storage_modules_guard,
            block.height,
            tx_index,
            touched,
        )?;

        prev_chunk_offset += num_chunks_in_tx as u64;
    }

    Ok(())
}

/// Bodies loaded for one migration window.
struct LoadedMigrationChunks {
    bodies: Vec<Option<UnpackedChunk>>,
    /// A Submit replica could not be read and at least one chunk is still
    /// empty. That chunk may sit on the skipped replica, so the pass is not
    /// "no local source".
    submit_unread: bool,
}

/// Source `count` contiguous transaction offsets starting at `tx_start`.
/// Cache hits stay per chunk. Each Publish cache miss is resolved on every
/// Submit replica. A replica whose read fails is skipped; the others are
/// still read. A hard fallback error keeps the chunks already loaded. Term
/// ledgers do not fall back across ledgers. `None` is a hole.
fn load_chunks_for_migration(
    storage_modules_guard: &StorageModulesReadGuard,
    db: &DatabaseProvider,
    target_ledger: DataLedger,
    data_commitment: DataCommitment,
    data_tree_root: DataTreeRoot,
    data_size: u64,
    tx_start: TxChunkOffset,
    count: usize,
    config: &Config,
) -> Result<LoadedMigrationChunks, MigrationError> {
    let geometry = config.data_tree_geometry();
    let mut bodies = vec![None; count];
    let mut misses = Vec::new();
    for step in 0..count {
        let tx_offset = tx_offset_at(tx_start, step)?;
        match get_cached_chunk(db, data_commitment, tx_offset) {
            Ok(Some((_metadata, cached))) if cached.chunk.is_some() => {
                match validate_chunk_for_migration(
                    cached,
                    data_tree_root,
                    data_size,
                    tx_offset,
                    &geometry,
                ) {
                    Ok(chunk) => bodies[step] = Some(chunk),
                    Err(error) => {
                        tracing::warn!(
                            data_commitment = %data_commitment,
                            %tx_offset,
                            ?error,
                            "Cached chunk failed migration validation; checking durable fallback"
                        );
                        misses.push(step);
                    }
                }
            }
            Ok(_) => misses.push(step),
            Err(error) => {
                tracing::warn!(
                    data_commitment = %data_commitment,
                    %tx_offset,
                    ?error,
                    "Failed to read cached chunk during migration; checking durable fallback"
                );
                misses.push(step);
            }
        }
    }
    let mut submit_unread = false;
    if target_ledger == DataLedger::Publish && !misses.is_empty() {
        match fill_submit_misses(
            storage_modules_guard,
            data_commitment,
            data_tree_root,
            data_size,
            tx_start,
            &misses,
            &mut bodies,
            &geometry,
        ) {
            Ok(unread) => submit_unread = unread,
            Err(error) => {
                tracing::warn!(
                    data_commitment = %data_commitment,
                    ?error,
                    "Submit fallback failed; keeping chunks already loaded"
                );
            }
        }
    }
    Ok(LoadedMigrationChunks {
        bodies,
        submit_unread,
    })
}

fn tx_offset_at(tx_start: TxChunkOffset, step: usize) -> Result<TxChunkOffset, MigrationError> {
    let start = *tx_start;
    let step = u32::try_from(step)
        .map_err(|_| MigrationError::Other(format!("tx offset step {step} exceeds u32")))?;
    let raw = start
        .checked_add(step)
        .ok_or_else(|| MigrationError::Other(format!("tx offset {start} + {step} exceeds u32")))?;
    Ok(TxChunkOffset::from(raw))
}

/// Each still-empty index is resolved on every Submit replica. A tx offset
/// this replica does not hold is a miss for the next replica. Adding later
/// indexes to the first placement reads past the partition and fails the
/// chunks that were in range. A read error skips that replica; later
/// replicas are still read. `Ok(true)` means a replica could not be read
/// and a miss is still empty: that chunk may sit on the skipped replica.
fn fill_submit_misses(
    storage_modules_guard: &StorageModulesReadGuard,
    data_commitment: DataCommitment,
    data_tree_root: DataTreeRoot,
    data_size: u64,
    tx_start: TxChunkOffset,
    misses: &[usize],
    bodies: &mut [Option<UnpackedChunk>],
    geometry: &Geometry,
) -> Result<bool, MigrationError> {
    let storage_modules = storage_modules_guard.read().clone();
    let mut pending = misses.to_vec();
    let mut submit_unread = false;
    for module in &storage_modules {
        if pending.is_empty() {
            break;
        }
        let is_submit = module
            .partition_assignment()
            .and_then(|assignment| assignment.ledger_id)
            == Some(DataLedger::Submit as u32);
        if !is_submit {
            continue;
        }
        let mut still = Vec::new();
        let mut skip_module = false;
        for step in pending {
            if skip_module {
                still.push(step);
                continue;
            }
            let tx_offset = tx_offset_at(tx_start, step)?;
            let placements = match irys_domain::trace_index_read(irys_domain::MIGRATION, || {
                module.partition_offsets_for_commitment_chunk(data_commitment, tx_offset)
            }) {
                Ok(placements) => placements,
                Err(error) => {
                    tracing::warn!(
                        data_commitment = %data_commitment,
                        %tx_offset,
                        storage_module.id = module.id,
                        ?error,
                        "Submit module index read failed; skipping module"
                    );
                    skip_module = true;
                    still.push(step);
                    continue;
                }
            };
            let Some(placements) = placements else {
                still.push(step);
                continue;
            };
            if placements.is_empty() {
                still.push(step);
                continue;
            }
            let mut filled = false;
            for place in placements {
                let read = match module.read_durable_bodies(place, place) {
                    Ok(read) => read,
                    Err(error) => {
                        tracing::warn!(
                            data_commitment = %data_commitment,
                            %tx_offset,
                            storage_module.id = module.id,
                            partition.offset = %place,
                            ?error,
                            "Submit module body read failed; skipping module"
                        );
                        skip_module = true;
                        break;
                    }
                };
                let Some(body) = read.into_iter().next().flatten() else {
                    continue;
                };
                if body.data_commitment(geometry) != data_commitment || body.tx_offset != tx_offset
                {
                    tracing::warn!(
                        data_commitment = %data_commitment,
                        %tx_offset,
                        storage_module.id = module.id,
                        partition.offset = %place,
                        "Durable Submit chunk identity mismatch; checking another replica"
                    );
                    continue;
                }
                match validate_chunk_parts_for_migration(
                    body.data_path,
                    body.bytes,
                    data_tree_root,
                    data_size,
                    tx_offset,
                    geometry,
                ) {
                    Ok(chunk) => {
                        bodies[step] = Some(chunk);
                        filled = true;
                        break;
                    }
                    Err(error) => {
                        tracing::warn!(
                            data_commitment = %data_commitment,
                            %tx_offset,
                            storage_module.id = module.id,
                            partition.offset = %place,
                            ?error,
                            "Durable Submit chunk failed migration validation; checking another replica"
                        );
                    }
                }
            }
            if !filled {
                still.push(step);
            }
        }
        if skip_module {
            // The skipped replica may hold a chunk this pass did not read.
            submit_unread = true;
        }
        pending = still;
    }
    // A later replica can fill every miss. Only a miss that is still empty
    // may sit on the replica this pass could not read.
    Ok(submit_unread && !pending.is_empty())
}

/// Ledger offset where `block` starts appending to `ledger`.
///
/// Height 0 starts at 0. The previous block's `total_chunks` is the next
/// 0-indexed offset. The previous block may legitimately lack this ledger
/// entirely — e.g. a Cascade term ledger (OneYear/ThirtyDay) whose first
/// block sits right after the prior block's epoch predates activation.
/// Treat a missing entry as zero chunks (the ledger started at this block)
/// instead of indexing into it, which would panic.
///
/// A missing previous block index item is an error, not offset 0: `get_item`
/// is `None` both for a missing row and for a failed view, and either would
/// place this block's txs at the start of the ledger.
#[tracing::instrument(level = "trace", skip_all, fields(block.height = block.height, ledger = ?ledger))]
fn ledger_start_offset(
    block: &IrysBlockHeader,
    ledger: DataLedger,
    block_index: &BlockIndex,
) -> Result<LedgerChunkOffset, MigrationError> {
    if block.height == 0 {
        return Ok(LedgerChunkOffset::from(0));
    }
    let Some(prev) = block_index.get_item(block.height - 1) else {
        return Err(MigrationError::Other(format!(
            "block index item at height {} could not be read; not indexing {ledger:?} at offset 0",
            block.height - 1
        )));
    };
    let start_chunk_offset = prev
        .ledgers
        .iter()
        .find(|item| item.ledger == ledger)
        .map_or(0, |item| item.total_chunks);
    Ok(LedgerChunkOffset::from(start_chunk_offset))
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
    tx_index: u32,
    touched: &mut Vec<Arc<StorageModule>>,
) -> Result<(), MigrationError> {
    let overlapped_modules =
        get_overlapped_storage_modules(storage_modules_guard, ledger, &tx_chunk_range);

    for storage_module in overlapped_modules {
        storage_module
            .index_transaction_data_in(
                data_tx,
                &tx_path_proof.to_vec(),
                tx_chunk_range,
                block_height,
                Some(ledger),
                Some(tx_index),
            )
            .map_err(|e| {
                error!(
                    "Failed to add tx path + data_root + start_offset to index: {}",
                    e
                );
                MigrationError::ChunkIndexWrite
            })?;
        touched.push(storage_module);
    }
    Ok(())
}

fn get_cached_chunk(
    db: &DatabaseProvider,
    data_commitment: DataCommitment,
    chunk_offset: TxChunkOffset,
) -> eyre::Result<Option<(CachedChunkIndexMetadata, CachedChunk)>> {
    db.view_eyre(|tx| cached_chunk_by_chunk_offset(tx, data_commitment, chunk_offset))
}

#[cfg(test)]
#[tracing::instrument(level = "trace", skip_all, err)]
fn write_chunk_to_module(
    storage_module: &Arc<StorageModule>,
    chunk: &UnpackedChunk,
) -> Result<(), MigrationError> {
    storage_module
        .deposit_data_chunk(chunk)
        .map_err(|e| match e {
            // Recovery holds the module's data writes; the worker defers, so this
            // is expected and not an error worth an error-level log.
            WriteDataChunkError::WritesPaused => MigrationError::WritesPaused,
            e => {
                error!(
                    "Failed to write chunk for data_root {:?} chunk_offset {} data_size {}: {:?}",
                    chunk.data_tree_root, chunk.tx_offset, chunk.data_size, e
                );
                MigrationError::ChunkDataWrite
            }
        })
}

fn write_chunks_to_module(
    storage_module: &Arc<StorageModule>,
    chunks: &[UnpackedChunk],
) -> Result<Vec<BatchEnqueueItem>, MigrationError> {
    storage_module
        .deposit_data_chunks(chunks)
        .map_err(|e| match e {
            WriteDataChunkError::WritesPaused => MigrationError::WritesPaused,
            e => {
                error!("Failed to write {} chunk bodies: {:?}", chunks.len(), e);
                MigrationError::ChunkDataWrite
            }
        })
}

pub(crate) fn validate_chunk_for_migration(
    cached: CachedChunk,
    data_tree_root: DataTreeRoot,
    data_size: u64,
    chunk_offset: TxChunkOffset,
    geometry: &Geometry,
) -> Result<UnpackedChunk, MigrationError> {
    let bytes = cached.chunk.ok_or_else(|| {
        MigrationError::Other(format!(
            "cached chunk body missing for {} offset {}",
            data_tree_root, chunk_offset
        ))
    })?;
    validate_chunk_parts_for_migration(
        cached.data_path,
        bytes,
        data_tree_root,
        data_size,
        chunk_offset,
        geometry,
    )
}

pub(crate) fn validate_chunk_parts_for_migration(
    data_path: Base64,
    bytes: Base64,
    data_tree_root: DataTreeRoot,
    data_size: u64,
    chunk_offset: TxChunkOffset,
    geometry: &Geometry,
) -> Result<UnpackedChunk, MigrationError> {
    verify_data_chunk(
        geometry,
        data_commitment(geometry, data_tree_root, data_size),
        data_size,
        chunk_offset,
        &bytes.0,
        &data_path.0,
    )
    .map_err(|error| {
        MigrationError::Other(format!(
            "chunk failed revalidation for {} offset {}: {error}",
            data_tree_root, chunk_offset
        ))
    })?;
    Ok(UnpackedChunk {
        data_tree_root,
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
                service_senders.data_sync.clone(),
            )
            .run(),
        );

        let handle = runtime_handle.spawn(async move {
            let chunk_migration_service = Self {
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
            chunk_migration_service
                .start()
                .await
                .expect("Chunk migration service encountered an irrecoverable error")
        });

        TokioServiceHandle {
            name: "chunk_migration_service".to_string(),
            handle,
            shutdown_signal: shutdown_tx,
        }
    }

    #[tracing::instrument(name = "chunk_migration_service_start", level = "trace", skip_all, err)]
    async fn start(mut self) -> eyre::Result<()> {
        tracing::info!("starting chunk migration service");

        loop {
            tokio::select! {
                biased;

                _ = &mut self.shutdown => {
                    tracing::info!("Shutdown signal received for chunk migration service");
                    break;
                }

                msg = self.msg_rx.recv() => {
                    match msg {
                        Some(traced) => {
                            let (msg, span) = traced.into_parts();
                            self.inner.handle_message(msg).instrument(span).await;
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
            let (msg, span) = traced.into_parts();
            self.inner.handle_message(msg).instrument(span).await;
        }

        tracing::info!("shutting down chunk migration service gracefully");
        Ok(())
    }
}

/// Test gate between the shared recovery lock and the block-hash read.
///
/// Inert unless the indexer thread installs a guard. The flag is set only
/// after `lock_index_recovery_shared` returns on that thread. Another
/// thread's index path does not see the guard, so it does not sleep here
/// and does not set the flag.
#[cfg(test)]
mod recovery_lock_pause {
    use std::cell::RefCell;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    struct State {
        reached: AtomicBool,
        release: AtomicBool,
    }

    thread_local! {
        static ACTIVE: RefCell<Option<Arc<State>>> = const { RefCell::new(None) };
    }

    pub(super) struct Guard(Arc<State>);

    impl Guard {
        pub(super) fn arm() -> Self {
            Self(Arc::new(State {
                reached: AtomicBool::new(false),
                release: AtomicBool::new(false),
            }))
        }

        /// Install this gate on the current thread. The indexer thread calls
        /// this before it indexes. Other threads keep an empty slot.
        pub(super) fn enter_on_this_thread(&self) {
            ACTIVE.with(|slot| *slot.borrow_mut() = Some(Arc::clone(&self.0)));
        }

        /// `true` once this gate's indexer holds the shared lock and has not read the hash.
        pub(super) fn wait_until_reached(&self, bound: Duration) -> bool {
            let deadline = Instant::now() + bound;
            while Instant::now() < deadline {
                if self.0.reached.load(Ordering::SeqCst) {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            self.0.reached.load(Ordering::SeqCst)
        }

        pub(super) fn release(&self) {
            self.0.release.store(true, Ordering::SeqCst);
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            self.release();
            ACTIVE.with(|slot| {
                let mut active = slot.borrow_mut();
                if active
                    .as_ref()
                    .is_some_and(|state| Arc::ptr_eq(state, &self.0))
                {
                    *active = None;
                }
            });
        }
    }

    pub(super) fn pause_after_shared_lock() {
        let Some(state) = ACTIVE.with(|slot| slot.borrow().clone()) else {
            return;
        };
        state.reached.store(true, Ordering::SeqCst);
        while !state.release.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::body_worker::{BodyMigrationWorker, MAX_BODY_MIGRATION_ATTEMPTS};
    use super::*;
    use irys_database::{
        IrysDatabaseArgs as _, cache_chunk, cache_data_root, insert_block_header, insert_tx_header,
        open_or_create_db, submodule::tables::PendingBodyMigration, tables::IrysTables,
    };
    use irys_domain::{ChunkType, StorageModuleInfo};
    use irys_storage::{ie, ii};
    use irys_testing_utils::TempDirBuilder;
    use irys_types::{
        BlockIndexItem, ConsensusConfig, DataTransaction, DataTransactionHeaderV1,
        DataTransactionHeaderV1WithMetadata, DataTransactionMetadata, H256List, IrysAddress,
        LedgerChunkOffset, LedgerChunkRange, LedgerIndexItem, NodeConfig, PartitionChunkOffset,
        SubmoduleIndexEngine, irys::IrysSigner, partition::PartitionAssignment,
        partition_chunk_offset_ie,
    };
    use std::{
        sync::{
            Mutex, RwLock,
            atomic::{AtomicBool, Ordering},
        },
        task::{Context, Poll, Waker},
        time::Duration,
    };

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
    // MDBX-SUBMODULE-INDEX: this helper opens only MDBX. Retarget it at RocksDB, or delete the tests that call fixture.
    fn fixture(num_chunks: u64, max_pending_write_bytes: Option<u64>) -> eyre::Result<Fixture> {
        fixture_with(
            num_chunks,
            max_pending_write_bytes,
            SubmoduleIndexEngine::Mdbx,
            20,
        )
    }

    fn fixture_with(
        num_chunks: u64,
        max_pending_write_bytes: Option<u64>,
        engine: SubmoduleIndexEngine,
        partition_chunks: u64,
    ) -> eyre::Result<Fixture> {
        let tmp = TempDirBuilder::new().with_tracing().build();
        let chunk_size = 32_u64;
        let mut node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size,
                num_chunks_in_partition: partition_chunks,
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
        node_config.database.submodule_index = engine;
        if engine == SubmoduleIndexEngine::Rocksdb {
            // The ordering test must observe the block sync, not the backstop.
            node_config.storage.index_wal_backstop_ms = 60_000;
        }
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
                submodules: vec![(
                    partition_chunk_offset_ie!(0, partition_chunks as u32),
                    "hdd0".into(),
                )],
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
        ///
        /// The block must already be the index item at this height. Production
        /// migration checks that. Tests that call this helper do not.
        fn index(&self) -> Result<(), MigrationError> {
            self.ensure_block_indexed()
                .map_err(|err| MigrationError::Other(err.to_string()))?;
            index_fixture_block(self)
        }

        fn ensure_block_indexed(&self) -> eyre::Result<()> {
            if self
                .block_index
                .get_item(self.block.height)
                .is_some_and(|item| item.block_hash == self.block.block_hash)
            {
                return Ok(());
            }
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

        /// Put every chunk body of the tx into the node's chunk cache.
        fn seed_cache(&self) -> eyre::Result<()> {
            self.db.update_eyre(|wtx| {
                cache_data_root(
                    wtx,
                    &self.tx.header,
                    None,
                    &self.config.data_tree_geometry(),
                )?;
                for chunk in self.tx.data_chunks()? {
                    cache_chunk(wtx, &chunk, &self.config.data_tree_geometry())?;
                }
                Ok(())
            })
        }

        /// A fresh worker over the same modules and DB — also what a restart looks like.
        fn worker(&self) -> BodyMigrationWorker {
            let (senders, _receivers) = crate::services::ServiceSenders::new();
            self.worker_sending_to(senders)
        }

        fn worker_sending_to(
            &self,
            senders: crate::services::ServiceSenders,
        ) -> BodyMigrationWorker {
            BodyMigrationWorker::new(
                self.guard.clone(),
                self.db.clone(),
                self.config.clone(),
                Arc::new(Notify::new()),
                self.shutdown.clone(),
                senders.data_sync.clone(),
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
    #[tokio::test]
    async fn heal_reindex_is_index_only() -> eyre::Result<()> {
        let f = fixture(3, Some(u64::MAX))?;
        f.seed_cache()?;
        f.persist_block()?;
        let mut inner = f.service_inner();

        inner
            .on_update_storage_module_indexes(f.block.block_hash)
            .await?;

        assert_eq!(f.rows()?.len(), 1);
        assert!(!f.sm.has_pending_writes());
        assert_ne!(f.chunk_type(0), Some(ChunkType::Data));
        assert!(
            f.sm.partition_offsets_for_commitment_chunk(
                f.tx.header.data_commitment(&f.config.data_tree_geometry()),
                TxChunkOffset::from(0)
            )?
            .is_some()
        );
        Ok(())
    }

    /// A block whose migration fails is logged and skipped; the next block is
    /// still processed. Previously the error unwound `start()` and killed the
    /// service for the life of the node.
    #[tokio::test]
    async fn failed_block_does_not_stop_later_blocks() -> eyre::Result<()> {
        let f = fixture(3, Some(u64::MAX))?;
        f.persist_block()?;
        let mut inner = f.service_inner();

        // Same hash (passes the block-index guard), corrupted tx_root: merklization fails.
        let mut bad = (*f.block).clone();
        bad.data_ledgers[DataLedger::Submit].tx_root = H256::random();
        inner.handle_message(f.migrated_msg(Arc::new(bad))).await;
        assert!(f.rows()?.is_empty(), "failed block must index nothing");

        inner.handle_message(f.migrated_msg(f.block.clone())).await;
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
        assert_eq!(
            rows[0].1.data_commitment,
            f.tx.header.data_commitment(&f.config.data_tree_geometry())
        );
        assert_eq!(rows[0].1.attempts, 0);

        assert!(!f.sm.has_pending_writes());
        assert_ne!(f.chunk_type(0), Some(ChunkType::Data));
        assert_eq!(
            f.sm.partition_offsets_for_commitment_chunk(
                f.tx.header.data_commitment(&f.config.data_tree_geometry()),
                TxChunkOffset::from(2)
            )?,
            Some(vec![PartitionChunkOffset::from(2)])
        );
        Ok(())
    }

    /// The block at this height is in the index. The previous height is not.
    /// Indexing must fail and leave the module empty. Offset 0 is only correct
    /// when the previous item is present and has no entry for this ledger.
    #[test]
    fn missing_previous_block_index_item_indexes_nothing() -> eyre::Result<()> {
        let mut f = fixture(1, Some(u64::MAX))?;
        set_block_height(&mut f, 1);
        f.ensure_block_indexed()?;
        assert!(
            f.block_index.get_item(0).is_none(),
            "the previous height must be absent for this case"
        );

        let result = index_fixture_block(&f);
        let rows = f.rows()?;
        assert!(
            matches!(result, Err(MigrationError::Other(_))),
            "a missing previous block index item must not become ledger offset 0; wrote {rows:?}, got {result:?}"
        );
        assert!(
            rows.is_empty(),
            "a failed previous-item read must not write a pending body row; wrote {rows:?}"
        );
        assert!(
            f.sm.partition_offsets_for_commitment_chunk(
                f.tx.header.data_commitment(&f.config.data_tree_geometry()),
                TxChunkOffset::from(0)
            )?
            .is_none(),
            "a failed previous-item read must not write a tx path at offset 0"
        );
        Ok(())
    }

    /// A present previous item that has no Submit entry means this ledger
    /// starts here. The tx is indexed at offset 0.
    #[test]
    fn previous_item_without_this_ledger_indexes_at_zero() -> eyre::Result<()> {
        let mut f = fixture(1, Some(u64::MAX))?;
        set_block_height(&mut f, 1);
        f.block_index.push_item(
            &BlockIndexItem {
                block_hash: H256::random(),
                num_ledgers: 1,
                ledgers: vec![LedgerIndexItem {
                    total_chunks: 9,
                    tx_root: H256::zero(),
                    ledger: DataLedger::Publish,
                }],
            },
            0,
        )?;
        f.ensure_block_indexed()?;

        index_fixture_block(&f)?;

        let rows = f.rows()?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, PartitionChunkOffset::from(0));
        assert_eq!(rows[0].1.start_offset.0, 0);
        Ok(())
    }

    /// The previous item's chunk count for this ledger is this block's start.
    /// A 1-chunk tx after 4 existing chunks lands at partition offset 4.
    #[test]
    fn previous_ledger_total_is_the_start_offset() -> eyre::Result<()> {
        let mut f = fixture(1, Some(u64::MAX))?;
        set_block_height(&mut f, 1);
        let submit = &f.block.data_ledgers[DataLedger::Submit];
        f.block_index.push_item(
            &BlockIndexItem {
                block_hash: H256::random(),
                num_ledgers: 1,
                ledgers: vec![LedgerIndexItem {
                    total_chunks: 4,
                    tx_root: submit.tx_root,
                    ledger: DataLedger::Submit,
                }],
            },
            0,
        )?;
        f.ensure_block_indexed()?;

        index_fixture_block(&f)?;

        let rows = f.rows()?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, PartitionChunkOffset::from(4));
        assert_eq!(rows[0].1.start_offset.0, 4);
        assert_eq!(
            f.sm.partition_offsets_for_commitment_chunk(
                f.tx.header.data_commitment(&f.config.data_tree_geometry()),
                TxChunkOffset::from(0)
            )?,
            Some(vec![PartitionChunkOffset::from(4)])
        );
        Ok(())
    }

    fn set_block_height(f: &mut Fixture, height: u64) {
        let mut block = (*f.block).clone();
        block.height = height;
        f.block = Arc::new(block);
    }

    /// The indexer has the shared recovery lock and has not read the hash.
    /// Five seconds is the failure bound. The wait returns as soon as this
    /// test's indexer reaches the gate. Another thread must not count.
    ///
    /// Reaching the gate is not enough. The exclusive try must fail, so a
    /// pause that runs without the shared lock fails this check.
    fn wait_for_recovery_lock(
        pause: &super::recovery_lock_pause::Guard,
        module: &Arc<StorageModule>,
    ) {
        assert!(
            pause.wait_until_reached(Duration::from_secs(5)),
            "indexing must take the recovery lock before it reads the block hash"
        );
        assert_shared_recovery_lock_held(module);
    }

    /// The indexer is inside the pause, so the shared gate is already held.
    /// A free exclusive try means this pause ran without that gate.
    fn assert_shared_recovery_lock_held(module: &Arc<StorageModule>) {
        // The guard drops at the end of this statement. Holding it past the
        // pause deadlocks the indexer.
        let lock_is_held = module.try_lock_index_recovery_exclusive().is_none();
        if !lock_is_held {
            panic!("indexing must hold the shared recovery lock before it reads the block hash");
        }
    }

    /// A thread that did not install this gate must not sleep in it, and must
    /// not make `wait_until_reached` succeed.
    #[test]
    fn recovery_pause_ignores_another_thread() {
        let pause = super::recovery_lock_pause::Guard::arm();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let start = std::time::Instant::now();
            super::recovery_lock_pause::pause_after_shared_lock();
            let _ = tx.send(start.elapsed());
        });
        let elapsed = rx.recv_timeout(Duration::from_secs(2)).unwrap_or_else(|_| {
            panic!("another thread slept in this test's recovery pause");
        });
        assert!(
            elapsed < Duration::from_millis(200),
            "another thread slept in this test's recovery pause for {elapsed:?}"
        );
        assert!(
            !pause.wait_until_reached(Duration::from_millis(50)),
            "another thread must not satisfy this test's recovery pause"
        );
    }

    /// Recovery's pause sits after the shared lock and before the hash read.
    /// Replacing the hash there must keep the registration from committing.
    #[test]
    fn registration_started_under_recovery_does_not_commit_a_replaced_block() -> eyre::Result<()> {
        let f = fixture(3, Some(u64::MAX))?;
        f.persist_block()?;
        let pause = super::recovery_lock_pause::Guard::arm();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                pause.enter_on_this_thread();
                let _ = done_tx.send(index_fixture_block(&f));
            });
            wait_for_recovery_lock(&pause, &f.sm);
            let submit = &f.block.data_ledgers[DataLedger::Submit];
            f.block_index.push_item(
                &BlockIndexItem {
                    block_hash: H256::repeat_byte(9),
                    num_ledgers: 1,
                    ledgers: vec![LedgerIndexItem {
                        total_chunks: submit.total_chunks,
                        tx_root: submit.tx_root,
                        ledger: DataLedger::Submit,
                    }],
                },
                f.block.height,
            )?;
            pause.release();
            let result = done_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("index finishes after the recovery pause releases");
            result?;
            assert!(
                f.rows()?.is_empty(),
                "a block replaced before the hash read must not commit"
            );
            Ok(())
        })
    }

    /// The same pause still lets the registration commit when the hash is
    /// unchanged before the read.
    #[test]
    fn registration_after_recovery_commits_when_the_block_remains() -> eyre::Result<()> {
        let f = fixture(3, Some(u64::MAX))?;
        f.persist_block()?;
        let pause = super::recovery_lock_pause::Guard::arm();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                pause.enter_on_this_thread();
                let _ = done_tx.send(index_fixture_block(&f));
            });
            wait_for_recovery_lock(&pause, &f.sm);
            pause.release();
            let result = done_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("index finishes after the recovery pause releases");
            result?;
            assert_eq!(f.rows()?.len(), 1);
            Ok(())
        })
    }

    /// The block is not in the index. Indexing writes nothing.
    #[test]
    fn registration_skips_a_block_already_absent_from_the_index() -> eyre::Result<()> {
        let f = fixture(3, Some(u64::MAX))?;
        index_fixture_block(&f)?;
        assert!(
            f.rows()?.is_empty(),
            "a block that is not at this height must not commit"
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

        // Idle pass reads an empty backlog and publishes that zero.
        let idle = worker.drain_pass().await;
        assert_eq!(
            (
                idle.rows,
                idle.written,
                idle.settled,
                idle.retired,
                idle.recorded_gauges
            ),
            (0, 0, 0, 0, true)
        );
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
            f.sm.partition_offsets_for_commitment_chunk(
                f.tx.header.data_commitment(&f.config.data_tree_geometry()),
                TxChunkOffset::from(0)
            )?
            .is_some()
        );
        Ok(())
    }

    /// Two placements of one chunk. A reserved sibling makes the free
    /// placement refuse the enqueue. That pass is in flight: the row stays
    /// and the attempt count stays at zero for every pass the sibling is
    /// held. Releasing the sibling leaves both offsets entropy, and the next
    /// pass writes the body.
    #[tokio::test]
    async fn sibling_occupancy_does_not_retire_the_body_job() -> eyre::Result<()> {
        use irys_database::submodule::{SubmoduleStore as _, tables::DataRootInfo};
        use irys_types::RelativeChunkOffset;

        let f = fixture(1, Some(u64::MAX))?;
        f.sm.pack_with_zeros();
        f.seed_cache()?;
        f.index()?;

        let chunk_size = f.config.consensus.chunk_size;
        let (_, submodule) =
            f.sm.get_submodule_for_offset(PartitionChunkOffset::from(0))?;
        let sibling_root = H256::repeat_byte(0x42);
        submodule.db.update(|tx| {
            tx.add_data_root_info(
                f.tx.header.data_commitment(&f.config.data_tree_geometry()),
                &DataRootInfo {
                    start_offset: RelativeChunkOffset(1),
                    data_size: f.tx.header.data_size,
                    data_tree_root: f.tx.header.data_tree_root,
                },
            )?;
            tx.add_data_root_info(
                irys_types::data_tree::data_commitment(
                    &f.config.data_tree_geometry(),
                    DataTreeRoot(sibling_root),
                    chunk_size,
                ),
                &DataRootInfo {
                    start_offset: RelativeChunkOffset(1),
                    data_size: chunk_size,
                    data_tree_root: DataTreeRoot(sibling_root),
                },
            )
        })?;

        let queued = f.sm.deposit_data_chunks(&[UnpackedChunk {
            data_tree_root: DataTreeRoot(sibling_root),
            data_size: chunk_size,
            data_path: Base64(vec![1, 2, 3, 4]),
            bytes: Base64(vec![9_u8; chunk_size as usize]),
            tx_offset: TxChunkOffset::from(0),
        }])?;
        assert!(
            matches!(
                &queued[0],
                BatchEnqueueItem::Queued(offsets)
                    if offsets == &vec![PartitionChunkOffset::from(1)]
            ),
            "the sibling root must reserve only offset 1, got {queued:?}"
        );
        assert!(f.sm.is_data_write_pending_at(PartitionChunkOffset::from(1)));
        assert!(!f.sm.is_data_write_pending_at(PartitionChunkOffset::from(0)));
        assert_eq!(
            f.sm.partition_offsets_for_commitment_chunk(
                f.tx.header.data_commitment(&f.config.data_tree_geometry()),
                TxChunkOffset::from(0)
            )?,
            Some(vec![
                PartitionChunkOffset::from(0),
                PartitionChunkOffset::from(1),
            ])
        );

        let worker = f.worker();
        for _ in 0..MAX_BODY_MIGRATION_ATTEMPTS {
            let pass = worker.drain_pass().await;
            assert_eq!(
                pass.written, 0,
                "a reserved sibling must not enqueue this placement"
            );
            assert_eq!(
                pass.retired, 0,
                "a reserved sibling must not retire the job"
            );
        }
        let rows = f.rows()?;
        assert_eq!(rows.len(), 1, "the cache body is still owed");
        assert_eq!(rows[0].1.attempts, 0, "a reserved sibling is not a stall");
        assert_eq!(f.chunk_type(0), Some(ChunkType::Entropy));
        assert!(!f.sm.is_data_write_pending_at(PartitionChunkOffset::from(0)));

        // The sibling sweep ends. Both placements are still entropy.
        f.sm.drop_pending_writes_in_range(
            PartitionChunkOffset::from(1),
            PartitionChunkOffset::from(1),
        );
        assert!(!f.sm.is_data_write_pending_at(PartitionChunkOffset::from(1)));
        assert_eq!(f.chunk_type(1), Some(ChunkType::Entropy));

        let pass = worker.drain_pass().await;
        assert_eq!(
            pass.written, 1,
            "the free placement takes the body once the sibling is free"
        );
        assert_eq!(pass.retired, 0);
        assert_eq!(f.rows()?[0].1.attempts, 0);
        assert!(f.sm.is_data_write_pending_at(PartitionChunkOffset::from(0)));
        Ok(())
    }

    /// Replace the live label on the inclusive span. One cut, one insert.
    fn label_span(sm: &StorageModule, start: u32, end: u32, kind: ChunkType) {
        let mut intervals = sm.intervals().write().unwrap();
        let span = ii(
            PartitionChunkOffset::from(start),
            PartitionChunkOffset::from(end),
        );
        let _ = intervals.cut(span);
        intervals
            .insert_merge_touching_if_values_equal(span, kind)
            .unwrap_or_else(|_| panic!("interval {start}..={end} could not take {kind:?}"));
    }

    /// A reserved sibling sits outside the tx span. The tail is already Data.
    /// Each pass while the sibling is held must keep the job and must not
    /// read durability once per tail chunk. Releasing the sibling still
    /// writes the free placement. The pass after that write, while the head
    /// is queued, does not scan the tail either.
    #[tokio::test]
    async fn sibling_reserve_does_not_scan_a_durable_tail() -> eyre::Result<()> {
        use irys_database::submodule::{SubmoduleStore as _, tables::DataRootInfo};
        use irys_types::RelativeChunkOffset;

        const TX_CHUNKS: u64 = 1024;
        // A per-offset walk is one durability read per tail chunk. The head
        // and one lookahead read stay under this. The tail does not.
        const SCAN_BOUND: usize = 8;
        // MDBX-SUBMODULE-INDEX: retarget this test at RocksDB. The scan bound is not MDBX-specific.
        let f = fixture_with(
            TX_CHUNKS,
            Some(u64::MAX),
            SubmoduleIndexEngine::Mdbx,
            TX_CHUNKS + 1,
        )?;
        f.seed_cache()?;
        f.index()?;
        let tx_last = u32::try_from(TX_CHUNKS - 1)?;
        let sibling_at = u32::try_from(TX_CHUNKS)?;
        label_span(&f.sm, 1, tx_last, ChunkType::Data);
        label_span(&f.sm, 0, 0, ChunkType::Entropy);
        label_span(&f.sm, sibling_at, sibling_at, ChunkType::Entropy);
        assert_eq!(f.chunk_type(0), Some(ChunkType::Entropy));
        assert!(f.sm.is_data_chunk_durable_at(PartitionChunkOffset::from(1)));
        assert!(!f.sm.is_data_chunk_durable_at(PartitionChunkOffset::from(0)));

        let chunk_size = f.config.consensus.chunk_size;
        let (_, submodule) =
            f.sm.get_submodule_for_offset(PartitionChunkOffset::from(0))?;
        let sibling_root = H256::repeat_byte(0x42);
        let sibling_start = i32::try_from(sibling_at)?;
        submodule.db.update(|tx| {
            tx.add_data_root_info(
                f.tx.header.data_commitment(&f.config.data_tree_geometry()),
                &DataRootInfo {
                    start_offset: RelativeChunkOffset(sibling_start),
                    data_size: chunk_size,
                    data_tree_root: f.tx.header.data_tree_root,
                },
            )?;
            tx.add_data_root_info(
                irys_types::data_tree::data_commitment(
                    &f.config.data_tree_geometry(),
                    DataTreeRoot(sibling_root),
                    chunk_size,
                ),
                &DataRootInfo {
                    start_offset: RelativeChunkOffset(sibling_start),
                    data_size: chunk_size,
                    data_tree_root: DataTreeRoot(sibling_root),
                },
            )
        })?;
        let queued = f.sm.deposit_data_chunks(&[UnpackedChunk {
            data_tree_root: DataTreeRoot(sibling_root),
            data_size: chunk_size,
            data_path: Base64(vec![1, 2, 3, 4]),
            bytes: Base64(vec![9_u8; chunk_size as usize]),
            tx_offset: TxChunkOffset::from(0),
        }])?;
        assert!(
            matches!(
                &queued[0],
                BatchEnqueueItem::Queued(offsets)
                    if offsets == &vec![PartitionChunkOffset::from(sibling_at)]
            ),
            "the sibling root must reserve only the outside offset, got {queued:?}"
        );

        let worker = f.worker();
        for _ in 0..MAX_BODY_MIGRATION_ATTEMPTS {
            let pass = worker.drain_pass().await;
            assert!(
                pass.durable_reads < SCAN_BOUND,
                "a reserved sibling scanned the durable tail {} times",
                pass.durable_reads
            );
            assert_eq!(pass.written, 0);
            assert_eq!(pass.retired, 0);
            assert_eq!(pass.settled, 0);
        }
        let rows = f.rows()?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, PartitionChunkOffset::from(0));
        assert_eq!(rows[0].1.attempts, 0);
        assert_eq!(f.chunk_type(1), Some(ChunkType::Data));

        f.sm.drop_pending_writes_in_range(
            PartitionChunkOffset::from(sibling_at),
            PartitionChunkOffset::from(sibling_at),
        );
        let pass = worker.drain_pass().await;
        assert_eq!(
            pass.written, 1,
            "the free placement takes the body once the sibling is free"
        );
        assert!(
            pass.durable_reads < SCAN_BOUND,
            "writing the free placement scanned the durable tail {} times",
            pass.durable_reads
        );
        assert_eq!(pass.retired, 0);
        assert_eq!(pass.settled, 0);
        assert_eq!(f.rows()?[0].0, PartitionChunkOffset::from(0));
        assert_eq!(f.rows()?[0].1.attempts, 0);
        assert!(f.sm.is_data_write_pending_at(PartitionChunkOffset::from(0)));
        assert!(!f.sm.is_data_write_pending_at(PartitionChunkOffset::from(1)));

        // The head is queued and not durable. The tail is still one jump.
        let pass = worker.drain_pass().await;
        assert!(
            pass.durable_reads < SCAN_BOUND,
            "a queued head scanned the durable tail {} times",
            pass.durable_reads
        );
        assert_eq!(pass.written, 0);
        assert_eq!(pass.retired, 0);
        assert_eq!(pass.settled, 0);
        assert_eq!(f.rows()?[0].0, PartitionChunkOffset::from(0));
        assert_eq!(f.rows()?[0].1.attempts, 0);
        assert!(f.sm.is_data_write_pending_at(PartitionChunkOffset::from(0)));
        Ok(())
    }

    /// A queued body inside a Data span is not durable. The prefix is read
    /// per offset up to that hole. The suffix after the hole is one jump,
    /// not one durability read per chunk. The job stays on the hole.
    #[tokio::test]
    async fn pending_data_inside_a_run_does_not_scan_the_suffix() -> eyre::Result<()> {
        const TX_CHUNKS: u64 = 256;
        const HOLE: u32 = 4;
        // MDBX-SUBMODULE-INDEX: retarget this test at RocksDB. The scan bound is not MDBX-specific.
        let f = fixture_with(
            TX_CHUNKS,
            Some(u64::MAX),
            SubmoduleIndexEngine::Mdbx,
            TX_CHUNKS,
        )?;
        f.index()?;
        let last = u32::try_from(TX_CHUNKS - 1)?;
        label_span(&f.sm, 0, last, ChunkType::Data);
        let chunk_size = f.config.consensus.chunk_size as usize;
        assert!(
            f.sm.write_chunk(
                PartitionChunkOffset::from(HOLE),
                vec![1_u8; chunk_size],
                ChunkType::Data,
            ),
            "the hole must stay queued"
        );
        assert!(f.sm.is_data_chunk_durable_at(PartitionChunkOffset::from(0)));
        assert!(
            !f.sm
                .is_data_chunk_durable_at(PartitionChunkOffset::from(HOLE))
        );

        let pass = f.worker().drain_pass().await;
        assert_eq!(pass.settled, 0, "a queued hole is not durable");
        assert_eq!(pass.retired, 0);
        assert_eq!(pass.written, 0);
        assert_eq!(f.rows()?[0].0, PartitionChunkOffset::from(HOLE));
        assert_eq!(f.rows()?[0].1.attempts, 0);
        assert!(
            pass.durable_reads < 16,
            "the suffix after the hole was scanned {} times",
            pass.durable_reads
        );
        Ok(())
    }

    /// Retirement hands the entropy holes to data sync. A stall that has not
    /// reached the limit sends nothing.
    #[tokio::test]
    async fn worker_retirement_hands_entropy_holes_to_data_sync() -> eyre::Result<()> {
        let f = fixture(3, Some(u64::MAX))?;
        f.sm.pack_with_zeros();
        f.index()?;
        let (senders, mut receivers) = crate::services::ServiceSenders::new();
        let worker = f.worker_sending_to(senders);

        let pass = worker.drain_pass().await;
        assert_eq!(pass.retired, 0);
        assert!(receivers.data_sync.try_recv().is_err());

        for _ in 2..MAX_BODY_MIGRATION_ATTEMPTS {
            assert_eq!(worker.drain_pass().await.retired, 0);
        }
        let pass = worker.drain_pass().await;
        assert_eq!(pass.retired, 1);
        assert!(f.rows()?.is_empty());

        let traced = receivers
            .data_sync
            .try_recv()
            .expect("retired holes are handed to data sync");
        match traced.inner {
            crate::DataSyncServiceMessage::BodyMigrationHoles {
                storage_module_id,
                chunk_offsets,
            } => {
                assert_eq!(storage_module_id, f.sm.id);
                assert_eq!(
                    chunk_offsets,
                    vec![
                        PartitionChunkOffset::from(0),
                        PartitionChunkOffset::from(1),
                        PartitionChunkOffset::from(2),
                    ]
                );
            }
            _ => panic!("expected body migration holes"),
        }
        assert!(receivers.data_sync.try_recv().is_err());
        Ok(())
    }

    /// A retired span longer than the data-sync hot-slot budget sends only that
    /// many entropy offsets. The hole scan picks up the rest.
    #[tokio::test]
    async fn retired_entropy_holes_stop_at_the_pending_request_cap() -> eyre::Result<()> {
        let offsets = retire_entropy_holes(8, 3).await?;
        assert_eq!(offsets.len(), 3);
        assert_eq!(
            offsets,
            vec![
                PartitionChunkOffset::from(0),
                PartitionChunkOffset::from(1),
                PartitionChunkOffset::from(2),
            ]
        );
        Ok(())
    }

    /// A pending-request cap of zero still sends the first hole.
    #[tokio::test]
    async fn retired_entropy_holes_send_one_when_the_pending_cap_is_zero() -> eyre::Result<()> {
        let offsets = retire_entropy_holes(8, 0).await?;
        assert_eq!(offsets.len(), 1);
        assert_eq!(offsets, vec![PartitionChunkOffset::from(0)]);
        Ok(())
    }

    /// Pack, index, and stall one entropy span until retirement. Returns the
    /// offsets handed to data sync.
    async fn retire_entropy_holes(
        num_chunks: u64,
        max_pending_chunk_requests: u64,
    ) -> eyre::Result<Vec<PartitionChunkOffset>> {
        let f = fixture(num_chunks, Some(u64::MAX))?;
        f.sm.pack_with_zeros();
        f.index()?;
        let mut node_config = f.config.node_config.clone();
        node_config.data_sync.max_pending_chunk_requests = max_pending_chunk_requests;
        let config = Config::new(node_config, f.config.peer_id());
        let (senders, mut receivers) = crate::services::ServiceSenders::new();
        let worker = BodyMigrationWorker::new(
            f.guard.clone(),
            f.db.clone(),
            config,
            Arc::new(Notify::new()),
            f.shutdown.clone(),
            senders.data_sync.clone(),
        );

        assert_eq!(worker.drain_pass().await.retired, 0);
        for _ in 2..MAX_BODY_MIGRATION_ATTEMPTS {
            assert_eq!(worker.drain_pass().await.retired, 0);
        }
        assert_eq!(worker.drain_pass().await.retired, 1);

        let traced = receivers
            .data_sync
            .try_recv()
            .expect("retired holes are handed to data sync");
        match traced.inner {
            crate::DataSyncServiceMessage::BodyMigrationHoles { chunk_offsets, .. } => {
                Ok(chunk_offsets)
            }
            _ => panic!("expected body migration holes"),
        }
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

    /// A flush of the written prefix moves the pending key to the first offset
    /// that is not durable. The value stays the same. A fresh worker starts at
    /// that key, so the durable prefix is not walked again. Offsets still in
    /// `pending_writes` stay under the old key until they are durable.
    #[tokio::test]
    async fn worker_advances_the_pending_key_after_a_durable_prefix() -> eyre::Result<()> {
        let f = fixture(6, Some(u64::MAX))?;
        f.sm.pack_with_zeros();
        f.seed_cache()?;
        f.index()?;
        let original = f.rows()?[0].1.clone();

        let pass = f.worker().with_writes_per_pass(2).drain_pass().await;
        assert_eq!((pass.written, pass.settled), (2, 0));
        assert_eq!(
            f.rows()?[0].0,
            PartitionChunkOffset::from(0),
            "in-flight writes are not durable, so the key stays"
        );

        f.sm.force_sync_pending_chunks()?;
        let pass = f.worker().with_writes_per_pass(2).drain_pass().await;
        assert_eq!((pass.written, pass.settled), (2, 0));
        let rows = f.rows()?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, PartitionChunkOffset::from(2));
        assert_eq!(rows[0].1, original);
        assert!(f.sm.is_data_chunk_durable_at(PartitionChunkOffset::from(0)));
        assert!(f.sm.is_data_chunk_durable_at(PartitionChunkOffset::from(1)));
        assert!(f.sm.is_data_write_pending_at(PartitionChunkOffset::from(2)));
        assert!(!f.sm.is_data_write_pending_at(PartitionChunkOffset::from(0)));

        // Restart. The key is 2, so this pass does not enqueue 0 or 1 again.
        let pass = f.worker().with_writes_per_pass(2).drain_pass().await;
        assert_eq!((pass.written, pass.settled), (2, 0));
        assert_eq!(f.rows()?[0].0, PartitionChunkOffset::from(2));
        assert!(f.sm.is_data_write_pending_at(PartitionChunkOffset::from(4)));
        assert!(!f.sm.is_data_write_pending_at(PartitionChunkOffset::from(0)));

        f.sm.force_sync_pending_chunks()?;
        assert_eq!(f.worker().drain_pass().await.settled, 1);
        assert!(f.rows()?.is_empty());
        for offset in 0..6 {
            assert_eq!(f.chunk_type(offset), Some(ChunkType::Data));
        }
        Ok(())
    }

    /// One page per read, and a spent write budget stops the pass. Later jobs
    /// still drain on later passes. The full list API keeps every row.
    #[tokio::test]
    async fn worker_pages_pending_jobs_and_drains_them_all() -> eyre::Result<()> {
        let f = fixture(1, Some(u64::MAX))?;
        f.sm.pack_with_zeros();
        let chunk_size = f.config.consensus.chunk_size as usize;
        for at in 0_u64..3 {
            let data = vec![9_u8.wrapping_add(at as u8); chunk_size];
            let signer = IrysSigner::random_signer(&f.config.consensus);
            let tx =
                signer.sign_transaction(signer.create_transaction(data.clone(), H256::zero())?)?;
            f.db.update_eyre(|wtx| {
                cache_data_root(wtx, &tx.header, None, &f.config.data_tree_geometry())?;
                cache_chunk(wtx, &tx.data_chunks()?[0], &f.config.data_tree_geometry())?;
                Ok(())
            })?;
            f.sm.index_transaction_data(
                &tx.header,
                &vec![at as u8, 1, 2, 3],
                LedgerChunkRange(ie(
                    LedgerChunkOffset::from(at),
                    LedgerChunkOffset::from(at + 1),
                )),
                1,
            )?;
        }
        assert_eq!(f.rows()?.len(), 3, "the full list still returns every job");

        let worker = || f.worker().with_writes_per_pass(1).with_rows_per_pass(1);
        let pass = worker().drain_pass().await;
        assert_eq!((pass.rows, pass.written, pass.settled), (1, 1, 0));
        assert!(f.sm.is_data_write_pending_at(PartitionChunkOffset::from(0)));
        assert!(!f.sm.is_data_write_pending_at(PartitionChunkOffset::from(1)));
        assert_eq!(f.rows()?.len(), 3);

        f.sm.force_sync_pending_chunks()?;
        let pass = worker().drain_pass().await;
        assert_eq!((pass.settled, pass.written), (1, 1));
        assert!(!f.sm.is_data_write_pending_at(PartitionChunkOffset::from(2)));
        assert_eq!(f.rows()?.len(), 2);

        f.sm.force_sync_pending_chunks()?;
        let pass = worker().drain_pass().await;
        assert_eq!((pass.settled, pass.written), (1, 1));
        assert_eq!(f.rows()?.len(), 1);

        f.sm.force_sync_pending_chunks()?;
        assert_eq!(worker().drain_pass().await.settled, 1);
        assert!(f.rows()?.is_empty());
        for offset in 0..3 {
            assert_eq!(f.chunk_type(offset), Some(ChunkType::Data));
        }
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
        let expected =
            (2 * nwbs).max(256) * f.config.consensus.chunk_size * f.sm.submodule_count() as u64;
        assert_eq!(f.sm.pending_memory_ceiling_bytes(), expected);
        assert_eq!(
            expected,
            256 * f.config.consensus.chunk_size,
            "with num_writes_before_sync = 1 the floor is the ceiling"
        );

        let g = fixture(1, Some(12_345))?;
        assert_eq!(g.sm.pending_memory_ceiling_bytes(), 12_345);

        // A configured ceiling below one flush batch is raised to it: under that
        // level the threshold flush never fires and migration would crawl.
        let h = fixture(1, Some(1))?;
        let one_batch = h.config.node_config.storage.num_writes_before_sync
            * h.config.consensus.chunk_size
            * h.sm.submodule_count() as u64;
        assert_eq!(h.sm.pending_memory_ceiling_bytes(), one_batch);
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

    /// One uninitialized interval over a large span must be classified in one
    /// interval read. A per-offset `get_chunk_type` walk is what a crashed
    /// node repeats every tick until packing finishes.
    #[tokio::test]
    async fn worker_classifies_large_uninitialized_span_without_per_chunk_scan() -> eyre::Result<()>
    {
        const SPAN: u64 = 1_000_000;
        let chunk_size = 8_u64;
        let tmp = TempDirBuilder::new().with_tracing().build();
        let mut node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size,
                num_chunks_in_partition: SPAN,
                num_chunks_in_recall_range: 2,
                num_partitions_per_slot: 1,
                entropy_packing_iterations: 1,
                chain_id: 1,
                ..ConsensusConfig::testing()
            }),
            base_directory: tmp.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        node_config.storage.max_pending_write_bytes = Some(u64::MAX);
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
                submodules: vec![(partition_chunk_offset_ie!(0, SPAN), "hdd0".into())],
            },
            &config,
        )?);
        let data_root = H256::random();
        let data_tx = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                data_tree_root: DataTreeRoot(data_root),
                data_size: SPAN * chunk_size,
                ..Default::default()
            },
            metadata: DataTransactionMetadata::new(),
        });
        let last = u32::try_from(SPAN - 1)?;
        sm.index_transaction_data(
            &data_tx,
            &vec![1, 2, 3, 4],
            LedgerChunkRange(irys_types::ledger_chunk_offset_ii!(0_u32, last)),
            1,
        )?;

        let guard = StorageModulesReadGuard::new(Arc::new(RwLock::new(vec![sm.clone()])));
        let db = DatabaseProvider(Arc::new(open_or_create_db(
            tmp.path().join("irys_db"),
            IrysTables::ALL,
            reth_db::mdbx::DatabaseArguments::irys_testing()?,
        )?));
        let (_shutdown_signal, shutdown) = reth::tasks::shutdown::signal();
        let (senders, _receivers) = ServiceSenders::new();
        let worker = BodyMigrationWorker::new(
            guard,
            db,
            config,
            Arc::new(Notify::new()),
            shutdown,
            senders.data_sync.clone(),
        );

        let drain_started = std::time::Instant::now();
        let pass = worker.drain_pass().await;
        let drain = drain_started.elapsed();
        eprintln!(
            "large uninitialized span: drain_pass={drain:?} unwritable={} written={} chunk_type_reads={}",
            pass.unwritable, pass.written, pass.chunk_type_reads
        );
        assert_eq!(pass.written, 0);
        assert_eq!(pass.unwritable, SPAN as usize);
        assert_eq!(sm.pending_body_migrations()?[0].1.attempts, 0);
        assert_eq!(
            pass.chunk_type_reads, 0,
            "an uninitialized span is one interval jump; a per-offset scan calls get_chunk_type once per chunk"
        );
        Ok(())
    }

    /// A crash leaves the span as many short runs, not one. Jumping each run
    /// by scanning the run list from the start is quadratic in the fragment
    /// count and still misses the tick. The cursor must advance through the
    /// runs once.
    #[tokio::test]
    async fn worker_jumps_fragmented_uninitialized_span_in_one_pass() -> eyre::Result<()> {
        const SPAN: u64 = 40_000;
        let chunk_size = 8_u64;
        let tmp = TempDirBuilder::new().with_tracing().build();
        let mut node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size,
                num_chunks_in_partition: SPAN,
                num_chunks_in_recall_range: 2,
                num_partitions_per_slot: 1,
                entropy_packing_iterations: 1,
                chain_id: 1,
                ..ConsensusConfig::testing()
            }),
            base_directory: tmp.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        node_config.storage.max_pending_write_bytes = Some(u64::MAX);
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
                submodules: vec![(partition_chunk_offset_ie!(0, SPAN), "hdd0".into())],
            },
            &config,
        )?);
        {
            let mut intervals = sm.intervals().write().unwrap();
            for offset in (0..SPAN).step_by(2) {
                StorageModule::cut_then_insert_interval_if_touching(
                    &mut intervals,
                    PartitionChunkOffset::from(offset),
                    ChunkType::Interrupted,
                );
            }
        }
        let data_root = H256::random();
        let data_tx = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                data_tree_root: DataTreeRoot(data_root),
                data_size: SPAN * chunk_size,
                ..Default::default()
            },
            metadata: DataTransactionMetadata::new(),
        });
        let last = u32::try_from(SPAN - 1)?;
        sm.index_transaction_data(
            &data_tx,
            &vec![1, 2, 3, 4],
            LedgerChunkRange(irys_types::ledger_chunk_offset_ii!(0_u32, last)),
            1,
        )?;

        let guard = StorageModulesReadGuard::new(Arc::new(RwLock::new(vec![sm.clone()])));
        let db = DatabaseProvider(Arc::new(open_or_create_db(
            tmp.path().join("irys_db"),
            IrysTables::ALL,
            reth_db::mdbx::DatabaseArguments::irys_testing()?,
        )?));
        let (_shutdown_signal, shutdown) = reth::tasks::shutdown::signal();
        let (senders, _receivers) = ServiceSenders::new();
        let worker = BodyMigrationWorker::new(
            guard,
            db,
            config,
            Arc::new(Notify::new()),
            shutdown,
            senders.data_sync.clone(),
        );

        let drain_started = std::time::Instant::now();
        let pass = worker.drain_pass().await;
        let drain = drain_started.elapsed();
        eprintln!(
            "fragmented uninitialized span: drain_pass={drain:?} unwritable={} written={} classify_index_steps={}",
            pass.unwritable, pass.written, pass.classify_index_steps
        );
        assert_eq!(pass.written, 0);
        assert_eq!(pass.unwritable, SPAN as usize);
        assert_eq!(sm.pending_body_migrations()?[0].1.attempts, 0);
        assert!(
            pass.classify_index_steps >= SPAN as usize,
            "classify_run recorded {} index steps for {SPAN} runs",
            pass.classify_index_steps
        );
        assert!(
            pass.classify_index_steps <= SPAN as usize * 4,
            "classify_run recorded {} index steps for {SPAN} runs",
            pass.classify_index_steps
        );
        Ok(())
    }

    /// A paused write is `WritesPaused`, not `Other`. The worker matches that
    /// variant so a resume before a later state read cannot turn a pause into
    /// a stall.
    #[test]
    fn write_chunk_to_module_maps_pause_to_writes_paused() -> eyre::Result<()> {
        let f = fixture(1, Some(u64::MAX))?;
        f.sm.pack_with_zeros();
        f.index()?;
        let chunk = UnpackedChunk {
            data_tree_root: f.tx.header.data_tree_root,
            data_size: f.tx.header.data_size,
            data_path: f.tx.chunk_proof(TxChunkOffset::from(0_u32)).unwrap(),
            bytes: Base64(f.data.clone()),
            tx_offset: TxChunkOffset::from(0_u32),
        };
        f.sm.pause_data_writes();
        assert!(matches!(
            write_chunk_to_module(&f.sm, &chunk),
            Err(MigrationError::WritesPaused)
        ));
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

    /// Submit module on `slot`. Slot 0 overlaps the fixture tx. A later slot
    /// does not, so indexing leaves its pending table empty.
    fn submit_module(config: &Config, id: usize, slot: usize) -> eyre::Result<Arc<StorageModule>> {
        assigned_module(config, id, slot, DataLedger::Submit)
    }

    fn publish_module(config: &Config, id: usize, slot: usize) -> eyre::Result<Arc<StorageModule>> {
        assigned_module(config, id, slot, DataLedger::Publish)
    }

    fn assigned_module(
        config: &Config,
        id: usize,
        slot: usize,
        ledger: DataLedger,
    ) -> eyre::Result<Arc<StorageModule>> {
        Ok(Arc::new(StorageModule::new(
            &StorageModuleInfo {
                id,
                partition_assignment: Some(PartitionAssignment {
                    ledger_id: Some(ledger.into()),
                    slot_index: Some(slot),
                    miner_address: IrysAddress::from([0xBB; 20]),
                    partition_hash: H256::random(),
                }),
                submodules: vec![(partition_chunk_offset_ie!(0, 20), format!("hdd{id}").into())],
            },
            config,
        )?))
    }

    /// Drop body-migration rows. The index and any durable bytes stay.
    fn clear_pending(module: &StorageModule) -> eyre::Result<()> {
        for (offset, job) in module.pending_body_migrations()? {
            module.settle_pending_body_migration(offset, job.data_commitment, job.block_height)?;
        }
        Ok(())
    }

    /// Shrink `chunks.dat` so the next durable read hits a short file.
    /// `StorageSubmodule::path` is the directory name under the node base.
    fn truncate_chunk_file(config: &Config, module: &StorageModule) -> eyre::Result<()> {
        let (_, submodule) = module.get_submodule_for_offset(PartitionChunkOffset::from(0))?;
        let path = config
            .node_config
            .base_directory
            .join(&submodule.path)
            .join("chunks.dat");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)?
            .set_len(0)?;
        Ok(())
    }

    /// Height 0 Publish copy of the fixture tx, indexed onto `guard`.
    fn index_publish(f: &Fixture, guard: &StorageModulesReadGuard) -> Result<(), MigrationError> {
        f.ensure_block_indexed()
            .map_err(|err| MigrationError::Other(err.to_string()))?;
        let mut block = (*f.block).clone();
        let (tx_root, _) =
            DataTransactionLedger::merklize_tx_root(std::slice::from_ref(&f.tx.header));
        {
            let publish = &mut block.data_ledgers[DataLedger::Publish];
            publish.tx_root = tx_root;
            publish.total_chunks =
                f.tx.header
                    .data_size
                    .div_ceil(f.config.consensus.chunk_size);
            publish.tx_ids = H256List(vec![f.tx.header.id]);
        }
        process_ledger_transactions(
            &Arc::new(block),
            DataLedger::Publish,
            std::slice::from_ref(&f.tx.header),
            &f.block_index,
            &f.config,
            guard,
            &mut Vec::new(),
        )
    }

    fn index_fixture_block(f: &Fixture) -> Result<(), MigrationError> {
        process_ledger_transactions(
            &f.block,
            DataLedger::Submit,
            std::slice::from_ref(&f.tx.header),
            &f.block_index,
            &f.config,
            &f.guard,
            &mut Vec::new(),
        )
    }

    fn index_on(f: &Fixture, guard: &StorageModulesReadGuard) -> Result<(), MigrationError> {
        f.ensure_block_indexed()
            .map_err(|err| MigrationError::Other(err.to_string()))?;
        process_ledger_transactions(
            &f.block,
            DataLedger::Submit,
            std::slice::from_ref(&f.tx.header),
            &f.block_index,
            &f.config,
            guard,
            &mut Vec::new(),
        )
    }

    fn unpacked_at(f: &Fixture, index: usize) -> UnpackedChunk {
        f.tx.data_chunks().unwrap()[index].clone()
    }

    /// Real entropy, so a later data write unpacks to the original bytes.
    /// Zero padding does not: the reader recomputes entropy and the proof check fails.
    fn write_real_entropy(module: &StorageModule, config: &Config) -> eyre::Result<()> {
        let chunk_size = usize::try_from(config.consensus.chunk_size)?;
        let partition_hash = module
            .partition_hash()
            .ok_or_else(|| eyre::eyre!("module has no partition"))?;
        let chunks = u32::try_from(config.consensus.num_chunks_in_partition)?;
        for offset in 0..chunks {
            let mut entropy = Vec::with_capacity(chunk_size);
            irys_packing::capacity_single::compute_entropy_chunk(
                config.node_config.miner_address(),
                u64::from(offset),
                partition_hash.0,
                config.consensus.entropy_packing_iterations,
                chunk_size,
                &mut entropy,
                config.consensus.chain_id,
            );
            assert!(
                module.write_chunk(
                    PartitionChunkOffset::from(offset),
                    entropy,
                    ChunkType::Entropy,
                ),
                "entropy write at {offset}"
            );
        }
        module.force_sync_pending_chunks()?;
        Ok(())
    }

    /// Publish asks Submit for tx offsets 18, 19, and 20. Offset 19 is the last
    /// chunk of this partition and holds durable data. Offset 20 is the next
    /// partition. The in-range chunk and an earlier cache hit stay. The tail
    /// is a miss.
    #[test]
    fn submit_fallback_keeps_in_partition_chunk_when_run_crosses_boundary() -> eyre::Result<()> {
        let f = fixture(20, Some(u64::MAX))?;
        write_real_entropy(&f.sm, &f.config)?;
        f.index()?;
        f.sm.write_data_chunk(&unpacked_at(&f, 19))
            .map_err(|err| eyre::eyre!("write chunk 19: {err:?}"))?;
        f.sm.force_sync_pending_chunks()?;
        f.db.update_eyre(|wtx| {
            cache_data_root(wtx, &f.tx.header, None, &f.config.data_tree_geometry())?;
            cache_chunk(wtx, &unpacked_at(&f, 18), &f.config.data_tree_geometry())?;
            Ok(())
        })?;

        let bodies = load_chunks_for_migration(
            &f.guard,
            &f.db,
            DataLedger::Publish,
            f.tx.header.data_commitment(&f.config.data_tree_geometry()),
            f.tx.header.data_tree_root,
            f.tx.header.data_size,
            TxChunkOffset::from(18),
            3,
            &f.config,
        )?
        .bodies;
        assert_eq!(
            bodies[0].as_ref().map(|chunk| chunk.tx_offset),
            Some(TxChunkOffset::from(18)),
            "a cache hit loaded before the fallback must stay"
        );
        assert_eq!(
            bodies[1].as_ref().map(|chunk| chunk.tx_offset),
            Some(TxChunkOffset::from(19)),
            "the last in-partition chunk must be read"
        );
        assert!(
            bodies[2].is_none(),
            "the next partition is a miss, not an error"
        );
        Ok(())
    }

    /// The first Submit module has no data for the prefix. The tail module,
    /// already visited, holds the next tx offset. Each index is resolved on
    /// every module, so the tail byte is read.
    #[test]
    fn submit_fallback_reads_tail_partition_when_prefix_has_no_data() -> eyre::Result<()> {
        let f = fixture(21, Some(u64::MAX))?;
        let tail = submit_module(&f.config, 1, 1)?;
        let guard =
            StorageModulesReadGuard::new(Arc::new(RwLock::new(vec![tail.clone(), f.sm.clone()])));
        write_real_entropy(&f.sm, &f.config)?;
        write_real_entropy(&tail, &f.config)?;
        index_on(&f, &guard)?;
        tail.write_data_chunk(&unpacked_at(&f, 20))
            .map_err(|err| eyre::eyre!("write tail chunk: {err:?}"))?;
        tail.force_sync_pending_chunks()?;

        let mut bodies = vec![None, None];
        fill_submit_misses(
            &guard,
            f.tx.header.data_commitment(&f.config.data_tree_geometry()),
            f.tx.header.data_tree_root,
            f.tx.header.data_size,
            TxChunkOffset::from(19),
            &[0, 1],
            &mut bodies,
            &f.config.data_tree_geometry(),
        )?;
        assert!(bodies[0].is_none(), "the prefix partition is still entropy");
        assert_eq!(
            bodies[1].as_ref().map(|chunk| chunk.tx_offset),
            Some(TxChunkOffset::from(20)),
            "the tail partition holds this tx offset"
        );
        Ok(())
    }

    /// A run that stays inside one partition still fills every offset.
    #[test]
    fn submit_fallback_fills_a_run_inside_one_partition() -> eyre::Result<()> {
        let f = fixture(4, Some(u64::MAX))?;
        write_real_entropy(&f.sm, &f.config)?;
        f.index()?;
        for index in [0, 1] {
            f.sm.write_data_chunk(&unpacked_at(&f, index))
                .map_err(|err| eyre::eyre!("write chunk {index}: {err:?}"))?;
        }
        f.sm.force_sync_pending_chunks()?;

        let mut bodies = vec![None, None];
        fill_submit_misses(
            &f.guard,
            f.tx.header.data_commitment(&f.config.data_tree_geometry()),
            f.tx.header.data_tree_root,
            f.tx.header.data_size,
            TxChunkOffset::from(0),
            &[0, 1],
            &mut bodies,
            &f.config.data_tree_geometry(),
        )?;
        assert_eq!(
            bodies[0].as_ref().map(|chunk| chunk.tx_offset),
            Some(TxChunkOffset::from(0))
        );
        assert_eq!(
            bodies[1].as_ref().map(|chunk| chunk.tx_offset),
            Some(TxChunkOffset::from(1))
        );
        Ok(())
    }

    /// Publish migration, cache empty. The earlier Submit module's durable
    /// read fails. The later Submit module holds the body. That body is copied
    /// onto the Publish module.
    #[tokio::test]
    async fn submit_fallback_migrates_from_later_module_when_earlier_read_fails() -> eyre::Result<()>
    {
        let f = fixture(1, Some(u64::MAX))?;
        let broken = submit_module(&f.config, 1, 0)?;
        let healthy = submit_module(&f.config, 2, 0)?;
        let publish = publish_module(&f.config, 3, 0)?;
        publish.pack_with_zeros();
        write_real_entropy(&broken, &f.config)?;
        write_real_entropy(&healthy, &f.config)?;

        let submit_guard = StorageModulesReadGuard::new(Arc::new(RwLock::new(vec![
            broken.clone(),
            healthy.clone(),
        ])));
        index_on(&f, &submit_guard)?;
        let chunk = unpacked_at(&f, 0);
        broken
            .write_data_chunk(&chunk)
            .map_err(|err| eyre::eyre!("write broken replica: {err:?}"))?;
        healthy
            .write_data_chunk(&chunk)
            .map_err(|err| eyre::eyre!("write healthy replica: {err:?}"))?;
        broken.force_sync_pending_chunks()?;
        healthy.force_sync_pending_chunks()?;
        truncate_chunk_file(&f.config, &broken)?;
        let broken_read = broken
            .read_durable_bodies(PartitionChunkOffset::from(0), PartitionChunkOffset::from(0));
        assert!(
            broken_read.is_err(),
            "the earlier Submit module's body read must fail, got {broken_read:?}"
        );
        assert!(
            healthy
                .read_durable_bodies(PartitionChunkOffset::from(0), PartitionChunkOffset::from(0))?
                .into_iter()
                .next()
                .flatten()
                .is_some(),
            "the later Submit module holds the body"
        );
        clear_pending(&broken)?;
        clear_pending(&healthy)?;

        let guard = StorageModulesReadGuard::new(Arc::new(RwLock::new(vec![
            broken.clone(),
            healthy.clone(),
            publish.clone(),
        ])));
        index_publish(&f, &guard)?;
        assert_eq!(publish.pending_body_migrations()?.len(), 1);
        assert_eq!(publish.pending_body_migrations()?[0].1.attempts, 0);

        let pass = worker_for(&f, guard).drain_pass().await;
        assert_eq!(
            pass.written, 1,
            "the body on the later Submit module must migrate"
        );
        assert_eq!(pass.retired, 0);
        assert!(
            publish.is_data_write_pending_at(PartitionChunkOffset::from(0)),
            "Publish must queue the body copied from the later Submit module"
        );
        assert_eq!(
            publish.pending_body_migrations()?[0].1.attempts,
            0,
            "a copied body is progress, not a stall"
        );
        Ok(())
    }

    /// The only Submit replica cannot be read. That pass is not "no local
    /// source": the row stays, and the attempt count stays at zero, including
    /// after `MAX_BODY_MIGRATION_ATTEMPTS` passes.
    #[tokio::test]
    async fn unread_submit_replica_does_not_count_toward_retirement() -> eyre::Result<()> {
        let f = fixture(1, Some(u64::MAX))?;
        let broken = submit_module(&f.config, 1, 0)?;
        let publish = publish_module(&f.config, 3, 0)?;
        publish.pack_with_zeros();
        write_real_entropy(&broken, &f.config)?;
        index_on(
            &f,
            &StorageModulesReadGuard::new(Arc::new(RwLock::new(vec![broken.clone()]))),
        )?;
        broken
            .write_data_chunk(&unpacked_at(&f, 0))
            .map_err(|err| eyre::eyre!("write broken replica: {err:?}"))?;
        broken.force_sync_pending_chunks()?;
        truncate_chunk_file(&f.config, &broken)?;
        let broken_read = broken
            .read_durable_bodies(PartitionChunkOffset::from(0), PartitionChunkOffset::from(0));
        assert!(
            broken_read.is_err(),
            "the Submit module's body read must fail, got {broken_read:?}"
        );
        clear_pending(&broken)?;

        let guard = StorageModulesReadGuard::new(Arc::new(RwLock::new(vec![
            broken.clone(),
            publish.clone(),
        ])));
        index_publish(&f, &guard)?;
        let worker = worker_for(&f, guard);
        for pass_n in 1..=MAX_BODY_MIGRATION_ATTEMPTS {
            let pass = worker.drain_pass().await;
            assert_eq!(pass.written, 0, "pass {pass_n}");
            assert_eq!(pass.retired, 0, "pass {pass_n}");
            let rows = publish.pending_body_migrations()?;
            assert_eq!(
                rows.len(),
                1,
                "pass {pass_n} must keep the row while the Submit replica is unreadable"
            );
            assert_eq!(
                rows[0].1.attempts, 0,
                "pass {pass_n} must not count an unread Submit replica"
            );
        }
        Ok(())
    }

    /// Publish migration, cache empty. The earlier Submit module's index read
    /// fails. Its body is still on disk. The later Submit module holds the
    /// same body, and that body is copied onto the Publish module.
    #[tokio::test]
    async fn submit_fallback_migrates_when_earlier_index_read_fails() -> eyre::Result<()> {
        let f = fixture(1, Some(u64::MAX))?;
        let broken = submit_module(&f.config, 1, 0)?;
        let healthy = submit_module(&f.config, 2, 0)?;
        let publish = publish_module(&f.config, 3, 0)?;
        publish.pack_with_zeros();
        write_real_entropy(&broken, &f.config)?;
        write_real_entropy(&healthy, &f.config)?;

        let submit_guard = StorageModulesReadGuard::new(Arc::new(RwLock::new(vec![
            broken.clone(),
            healthy.clone(),
        ])));
        index_on(&f, &submit_guard)?;
        let chunk = unpacked_at(&f, 0);
        broken
            .write_data_chunk(&chunk)
            .map_err(|err| eyre::eyre!("write broken replica: {err:?}"))?;
        healthy
            .write_data_chunk(&chunk)
            .map_err(|err| eyre::eyre!("write healthy replica: {err:?}"))?;
        broken.force_sync_pending_chunks()?;
        healthy.force_sync_pending_chunks()?;
        clear_pending(&broken)?;
        clear_pending(&healthy)?;

        let guard = StorageModulesReadGuard::new(Arc::new(RwLock::new(vec![
            broken.clone(),
            healthy.clone(),
            publish.clone(),
        ])));
        index_publish(&f, &guard)?;
        broken.fail_data_root_index_reads(true);
        let index_read = broken.partition_offsets_for_commitment_chunk(
            f.tx.header.data_commitment(&f.config.data_tree_geometry()),
            TxChunkOffset::from(0),
        );
        assert!(
            index_read.is_err(),
            "the earlier Submit module's index read must fail, got {index_read:?}"
        );
        assert!(
            broken
                .read_durable_bodies(PartitionChunkOffset::from(0), PartitionChunkOffset::from(0))?
                .into_iter()
                .next()
                .flatten()
                .is_some(),
            "the earlier Submit module's body is still readable"
        );
        assert!(
            healthy
                .read_durable_bodies(PartitionChunkOffset::from(0), PartitionChunkOffset::from(0))?
                .into_iter()
                .next()
                .flatten()
                .is_some(),
            "the later Submit module holds the body"
        );
        assert_eq!(publish.pending_body_migrations()?.len(), 1);
        assert_eq!(publish.pending_body_migrations()?[0].1.attempts, 0);

        let pass = worker_for(&f, guard).drain_pass().await;
        assert_eq!(
            pass.written, 1,
            "the body on the later Submit module must migrate"
        );
        assert_eq!(pass.retired, 0);
        assert!(
            publish.is_data_write_pending_at(PartitionChunkOffset::from(0)),
            "Publish must queue the body copied from the later Submit module"
        );
        assert_eq!(
            publish.pending_body_migrations()?[0].1.attempts,
            0,
            "a copied body is progress, not a stall"
        );
        Ok(())
    }

    /// The only Submit replica's index cannot be read. The body is still on
    /// disk. That pass is not "no local source": the row stays, and the
    /// attempt count stays at zero, including after
    /// `MAX_BODY_MIGRATION_ATTEMPTS` passes.
    #[tokio::test]
    async fn unread_submit_index_does_not_count_toward_retirement() -> eyre::Result<()> {
        let f = fixture(1, Some(u64::MAX))?;
        let broken = submit_module(&f.config, 1, 0)?;
        let publish = publish_module(&f.config, 3, 0)?;
        publish.pack_with_zeros();
        write_real_entropy(&broken, &f.config)?;
        index_on(
            &f,
            &StorageModulesReadGuard::new(Arc::new(RwLock::new(vec![broken.clone()]))),
        )?;
        broken
            .write_data_chunk(&unpacked_at(&f, 0))
            .map_err(|err| eyre::eyre!("write broken replica: {err:?}"))?;
        broken.force_sync_pending_chunks()?;
        clear_pending(&broken)?;

        let guard = StorageModulesReadGuard::new(Arc::new(RwLock::new(vec![
            broken.clone(),
            publish.clone(),
        ])));
        index_publish(&f, &guard)?;
        broken.fail_data_root_index_reads(true);
        let index_read = broken.partition_offsets_for_commitment_chunk(
            f.tx.header.data_commitment(&f.config.data_tree_geometry()),
            TxChunkOffset::from(0),
        );
        assert!(
            index_read.is_err(),
            "the Submit module's index read must fail, got {index_read:?}"
        );
        assert!(
            broken
                .read_durable_bodies(PartitionChunkOffset::from(0), PartitionChunkOffset::from(0))?
                .into_iter()
                .next()
                .flatten()
                .is_some(),
            "the Submit module's body is still readable"
        );

        let worker = worker_for(&f, guard);
        for pass_n in 1..=MAX_BODY_MIGRATION_ATTEMPTS {
            let pass = worker.drain_pass().await;
            assert_eq!(pass.written, 0, "pass {pass_n}");
            assert_eq!(pass.retired, 0, "pass {pass_n}");
            let rows = publish.pending_body_migrations()?;
            assert_eq!(
                rows.len(),
                1,
                "pass {pass_n} must keep the row while the Submit index is unreadable"
            );
            assert_eq!(
                rows[0].1.attempts, 0,
                "pass {pass_n} must not count an unread Submit index"
            );
        }
        Ok(())
    }

    fn worker_for(f: &Fixture, guard: StorageModulesReadGuard) -> BodyMigrationWorker {
        let (senders, _receivers) = crate::services::ServiceSenders::new();
        BodyMigrationWorker::new(
            guard,
            f.db.clone(),
            f.config.clone(),
            Arc::new(Notify::new()),
            f.shutdown.clone(),
            senders.data_sync.clone(),
        )
    }

    /// No pending rows: the pass did sample the backlog, and the sample is 0.
    #[tokio::test]
    async fn empty_pass_records_a_zero_pending_gauge() -> eyre::Result<()> {
        let f = fixture(3, Some(u64::MAX))?;
        let pass = f.worker().drain_pass().await;
        assert!(pass.recorded_gauges);
        assert_eq!(pass.rows, 0);
        assert!(f.rows()?.is_empty());
        Ok(())
    }

    /// A pause with nothing queued is not a skipped backlog.
    #[tokio::test]
    async fn paused_module_with_no_pending_rows_still_records_zero() -> eyre::Result<()> {
        let f = fixture(3, Some(u64::MAX))?;
        f.sm.pause_data_writes();
        let pass = f.worker().drain_pass().await;
        assert!(
            pass.recorded_gauges,
            "a paused module with an empty pending table is not a skipped backlog"
        );
        assert_eq!(pass.rows, 0);
        assert!(f.rows()?.is_empty());
        Ok(())
    }

    /// A pass that reads jobs publishes that sample.
    #[tokio::test]
    async fn unpaused_pass_records_the_jobs_it_read() -> eyre::Result<()> {
        let f = fixture(3, Some(u64::MAX))?;
        f.sm.pack_with_zeros();
        f.seed_cache()?;
        f.index()?;
        let pass = f.worker().drain_pass().await;
        assert!(pass.recorded_gauges);
        assert_eq!((pass.rows, pass.written), (1, 3));
        Ok(())
    }

    /// Rollback holds every module that still has a pending row. The pass
    /// reads none of them, so it must not publish 0 over the last sample.
    #[tokio::test]
    async fn paused_nonempty_pass_does_not_replace_pending_gauges() -> eyre::Result<()> {
        let f = fixture(3, Some(u64::MAX))?;
        f.sm.pack_with_zeros();
        f.seed_cache()?;
        f.index()?;
        f.sm.pause_data_writes();
        let pass = f.worker().drain_pass().await;
        assert!(
            !pass.recorded_gauges,
            "every nonempty pending table was paused; gauges must stay at the last sample"
        );
        assert_eq!(
            (pass.rows, pass.written, pass.settled, pass.retired),
            (0, 0, 0, 0)
        );
        assert_eq!(f.rows()?.len(), 1);
        assert_eq!(f.rows()?[0].1.attempts, 0);
        assert!(!f.sm.has_pending_writes());
        Ok(())
    }

    /// An unpaused sibling with an empty table is not a sample of the paused
    /// backlog. The gauges stay.
    #[tokio::test]
    async fn paused_nonempty_stays_unsampled_when_siblings_have_no_pending() -> eyre::Result<()> {
        let f = fixture(3, Some(u64::MAX))?;
        f.seed_cache()?;
        let sibling = submit_module(&f.config, 1, 1)?;
        let guard = StorageModulesReadGuard::new(Arc::new(RwLock::new(vec![
            f.sm.clone(),
            sibling.clone(),
        ])));
        index_on(&f, &guard)?;
        assert_eq!(f.rows()?.len(), 1);
        assert!(sibling.pending_body_migrations()?.is_empty());
        f.sm.pause_data_writes();
        let pass = worker_for(&f, guard).drain_pass().await;
        assert!(
            !pass.recorded_gauges,
            "the only nonempty pending table was paused"
        );
        assert_eq!(pass.rows, 0);
        assert_eq!(f.rows()?[0].1.attempts, 0);
        assert!(sibling.pending_body_migrations()?.is_empty());
        Ok(())
    }

    /// One module is paused with rows. Another is read. Record that sample.
    /// The paused module is absent from it, and its row is untouched.
    #[tokio::test]
    async fn pass_records_modules_it_read_while_a_sibling_is_paused() -> eyre::Result<()> {
        let f = fixture(3, Some(u64::MAX))?;
        let sibling = submit_module(&f.config, 1, 0)?;
        f.sm.pack_with_zeros();
        sibling.pack_with_zeros();
        f.seed_cache()?;
        let guard = StorageModulesReadGuard::new(Arc::new(RwLock::new(vec![
            f.sm.clone(),
            sibling.clone(),
        ])));
        index_on(&f, &guard)?;
        assert_eq!(f.rows()?.len(), 1);
        assert_eq!(sibling.pending_body_migrations()?.len(), 1);
        f.sm.pause_data_writes();
        let pass = worker_for(&f, guard).drain_pass().await;
        assert!(
            pass.recorded_gauges,
            "a pass that read an unpaused module records that sample"
        );
        assert_eq!((pass.rows, pass.written), (1, 3));
        assert_eq!(f.rows()?.len(), 1);
        assert_eq!(f.rows()?[0].1.attempts, 0);
        assert!(!f.sm.has_pending_writes());
        assert_eq!(sibling.pending_body_migrations()?.len(), 1);
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
            &[f.tx.header.data_commitment(&f.config.data_tree_geometry())],
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

    fn notify_pending(notify: &Notify) -> bool {
        let mut waiting = std::pin::pin!(notify.notified());
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        matches!(waiting.as_mut().poll(&mut cx), Poll::Pending)
    }

    fn submit_txs(f: &Fixture) -> Arc<HashMap<DataLedger, Vec<DataTransactionHeader>>> {
        let mut txs = HashMap::new();
        txs.insert(DataLedger::Submit, vec![f.tx.header.clone()]);
        Arc::new(txs)
    }

    /// Registrations in one block stay unsynced until one WAL sync. The body
    /// wake and `OnBlockMigrated` run only after that sync.
    #[tokio::test(flavor = "current_thread")]
    async fn block_index_sync_precedes_migration_side_effects() -> eyre::Result<()> {
        let f = fixture_with(1, Some(u64::MAX), SubmoduleIndexEngine::Rocksdb, 20)?;
        f.persist_block()?;
        let db = {
            let (_, sub) =
                f.sm.get_submodule_for_offset(PartitionChunkOffset::from(0))?;
            sub.db.clone()
        };
        let syncs_before = db.wal_sync_count();
        let backstop_before = db.wal_backstop_sync_count();
        f.sm.index_transaction_data(
            &f.tx.header,
            &vec![1, 2, 3],
            LedgerChunkRange(ie(LedgerChunkOffset::from(0), LedgerChunkOffset::from(1))),
            f.block.height,
        )?;
        assert_eq!(
            db.wal_sync_count(),
            syncs_before,
            "a registration inside the block does not sync"
        );
        assert!(db.published_generation() > db.durable_generation());

        let (service_senders, receivers) = ServiceSenders::new();
        let cache_rx = Arc::new(Mutex::new(receivers.chunk_cache));
        let body_wakeup = Arc::new(Notify::new());
        let mut inner = ChunkMigrationServiceInner::new(
            f.block_index.clone(),
            &f.guard,
            f.db.clone(),
            service_senders,
            f.config.clone(),
            Arc::clone(&body_wakeup),
        );
        let hook_cache = Arc::clone(&cache_rx);
        let hook_wake = Arc::clone(&body_wakeup);
        let hook_db = db.clone();
        let saw = Arc::new(AtomicBool::new(false));
        let hook_saw = Arc::clone(&saw);
        // The hook runs on the thread that just synced the WAL.
        let worker_thread = std::thread::current().id();
        inner.set_index_durable_hook(move || {
            assert_ne!(
                std::thread::current().id(),
                worker_thread,
                "index WAL sync ran on the async worker"
            );
            assert!(hook_db.wal_sync_count() > syncs_before);
            assert!(hook_db.durable_generation() >= hook_db.published_generation());
            assert!(
                hook_cache.lock().expect("cache rx").try_recv().is_err(),
                "OnBlockMigrated sent before the sync"
            );
            assert!(
                notify_pending(&hook_wake),
                "body worker woken before the sync"
            );
            hook_saw.store(true, Ordering::SeqCst);
        });

        inner
            .on_block_migrated(Arc::clone(&f.block), submit_txs(&f))
            .await?;
        assert!(saw.load(Ordering::SeqCst), "sync hook did not run");
        assert_eq!(db.wal_sync_count(), syncs_before + 1);
        assert_eq!(db.wal_backstop_sync_count(), backstop_before + 1);
        let traced = cache_rx
            .lock()
            .expect("cache rx")
            .try_recv()
            .expect("OnBlockMigrated");
        match traced.inner {
            CacheServiceAction::OnBlockMigrated(height, None) => {
                assert_eq!(height, f.block.height);
            }
            other => panic!("unexpected cache message: {other:?}"),
        }
        assert!(cache_rx.lock().expect("cache rx").try_recv().is_err());
        assert!(!notify_pending(&body_wakeup));
        Ok(())
    }

    /// A failed per-block sync is the old registration-sync error, and it
    /// does not wake the body worker or send `OnBlockMigrated`.
    #[tokio::test]
    async fn block_index_sync_failure_does_not_publish() -> eyre::Result<()> {
        let f = fixture_with(1, Some(u64::MAX), SubmoduleIndexEngine::Rocksdb, 20)?;
        f.persist_block()?;
        let db = {
            let (_, sub) =
                f.sm.get_submodule_for_offset(PartitionChunkOffset::from(0))?;
            sub.db.clone()
        };
        db.fail_next_wal_sync();
        let (service_senders, receivers) = ServiceSenders::new();
        let mut cache_rx = receivers.chunk_cache;
        let body_wakeup = Arc::new(Notify::new());
        let mut inner = ChunkMigrationServiceInner::new(
            f.block_index.clone(),
            &f.guard,
            f.db.clone(),
            service_senders,
            f.config.clone(),
            Arc::clone(&body_wakeup),
        );
        let saw = Arc::new(AtomicBool::new(false));
        let hook_saw = Arc::clone(&saw);
        inner.set_index_durable_hook(move || {
            hook_saw.store(true, Ordering::SeqCst);
        });

        let err = inner
            .on_block_migrated(Arc::clone(&f.block), submit_txs(&f))
            .await
            .expect_err("sync failure");
        assert!(matches!(err, MigrationError::ChunkIndexWrite));
        assert!(
            !saw.load(Ordering::SeqCst),
            "side effects ran after a failed sync"
        );
        assert!(cache_rx.try_recv().is_err());
        assert!(notify_pending(&body_wakeup));
        assert!(db.wal_sync_failed());
        assert!(db.published_generation() > db.durable_generation());
        assert_eq!(f.rows()?.len(), 1);
        Ok(())
    }
}
