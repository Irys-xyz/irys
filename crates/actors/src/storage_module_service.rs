/// # StorageModuleService
///
/// Manages storage modules and their lifecycle within the node.
///
/// This service:
/// - Monitors and applies partition assignments from the network
/// - Initializes storage modules when assigned partitions
/// - Maintains the global registry of active storage modules
/// - Coordinates with the epoch service for runtime updates
/// - Handles dynamic addition/removal of storage modules
///
/// Acts as the central authority for storage module membership, with other
/// components accessing this information through read guards to ensure
/// consistency throughout the system.
use crate::{
    chunk_migration_service::ChunkMigrationServiceMessage, packing_service::PackingRequest,
    services::ServiceSenders, DataSyncServiceMessage,
};
use eyre::{eyre, OptionExt as _};
use irys_config::StorageSubmodulesConfig;
use irys_database::submodule::{get_path_hashes_by_offset, tables::ChunkPathHashes};
use irys_domain::{
    BlockIndexReadGuard, BlockTreeReadGuard, PackingParams, StorageModule, StorageModuleInfo,
    PACKING_PARAMS_FILE_NAME,
};
use irys_types::{
    BlockHash, Config, DataLedger, LedgerChunkOffset, PartitionChunkOffset, PartitionChunkRange,
    SendTraced as _, TokioServiceHandle, Traced,
};
use reth::tasks::shutdown::Shutdown;
use std::{
    collections::{BTreeMap, HashMap},
    ops::Range,
    path::Path,
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::sync::{mpsc::UnboundedReceiver /*, oneshot*/};
use tracing::{debug, error, warn, Instrument as _};

// Messages that the StorageModuleService service supports
#[derive(Debug)]
pub enum StorageModuleServiceMessage {
    PartitionAssignmentsUpdated {
        storage_module_infos: Arc<Vec<StorageModuleInfo>>,
        update_height: u64,
    },
}

#[derive(Debug)]
pub struct StorageModuleService {
    shutdown: Shutdown,
    msg_rx: UnboundedReceiver<Traced<StorageModuleServiceMessage>>,
    inner: StorageModuleServiceInner,
}

#[derive(Debug)]
pub struct StorageModuleServiceInner {
    storage_modules: Arc<RwLock<Vec<Arc<StorageModule>>>>,
    block_index: BlockIndexReadGuard,
    block_tree: BlockTreeReadGuard,
    submodules_config: StorageSubmodulesConfig,
    service_senders: ServiceSenders,
    config: Config,
}

impl StorageModuleServiceInner {
    /// Create a new StorageModuleServiceInner instance
    pub fn new(
        storage_modules: Arc<RwLock<Vec<Arc<StorageModule>>>>,
        block_index: BlockIndexReadGuard,
        block_tree: BlockTreeReadGuard,
        service_senders: ServiceSenders,
        config: Config,
    ) -> Self {
        let submodules_config =
            match StorageSubmodulesConfig::load(config.node_config.base_directory.clone()) {
                Ok(sm_config) => sm_config,
                Err(err) => panic!("{}", err),
            };

        Self {
            storage_modules,
            block_index,
            block_tree,
            submodules_config,
            service_senders,
            config,
        }
    }

    async fn handle_message(&mut self, msg: StorageModuleServiceMessage) -> eyre::Result<()> {
        match msg {
            StorageModuleServiceMessage::PartitionAssignmentsUpdated {
                storage_module_infos,
                update_height,
            } => {
                self.handle_partition_assignments_update(storage_module_infos, update_height)
                    .await?
            }
        }
        Ok(())
    }

    #[tracing::instrument(level = "trace", skip_all)]
    fn tick(&self) {
        // Check to see if any of the storage modules are ready to be flushed to disk
        let storage_modules = {
            let guard = self.storage_modules.read().unwrap();
            guard.clone()
        }; // <- Don't hold the read guard across an async boundary

        for sm in storage_modules.iter() {
            if sm.last_pending_write().elapsed() > Duration::from_secs(5) {
                if let Err(e) = sm.force_sync_pending_chunks() {
                    error!(
                        "Couldn't flush pending chunks for storage_module {}: {}",
                        sm.id, e
                    );
                }
            }
        }
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    async fn handle_partition_assignments_update(
        &mut self,
        storage_module_info_update: Arc<Vec<StorageModuleInfo>>,
        update_height: u64,
    ) -> eyre::Result<()> {
        // Read the current storage modules once, outside the loop
        // this is the current state of the storage modules prior of the partition assignments update
        let local_modules_by_id: HashMap<usize, Arc<StorageModule>> = {
            let modules_guard = self.storage_modules.read().unwrap();
            modules_guard
                .iter()
                .map(|module| (module.id, module.clone()))
                .collect()
        };
        let mut assigned_modules = Vec::new();
        let mut packing_modules = Vec::new();

        debug!("StorageModuleInfos:\n{:#?}", storage_module_info_update);

        let update_info_by_id: HashMap<usize, &StorageModuleInfo> = storage_module_info_update
            .iter()
            .map(|info| (info.id, info))
            .collect();

        for info in storage_module_info_update.iter() {
            if !local_modules_by_id.contains_key(&info.id) {
                eyre::bail!(
                    "StorageModuleInfo should only reference valid storage module ids - ID: {}, current info: {:#?}",
                    info.id, info
                );
            }
        }

        for (module_id, module) in local_modules_by_id.iter() {
            match update_info_by_id.get(module_id) {
                None => {
                    // storage module is present locally, not present in the update
                    self.clear_assignment_if_outdated(module, update_height, "missing_from_update");
                }
                Some(sm_info) => {
                    if sm_info.submodules.is_empty() {
                        return Err(eyre::eyre!(
                            "StorageModuleInfo {} missing submodule entries",
                            sm_info.id
                        ));
                    }

                    let path = &self.submodules_config.submodule_paths[sm_info.id];

                    if *path != sm_info.submodules[0].1 {
                        return Err(eyre::eyre!("Submodule paths don't match"));
                    }

                    if sm_info.partition_assignment.is_none() {
                        // storage module is present locally, present in the incoming update, but it has no partition assignment
                        self.clear_assignment_if_outdated(
                            module,
                            update_height,
                            "explicitly_unassigned",
                        );
                    }
                }
            }
        }

        for sm_info in storage_module_info_update.iter() {
            // Get the existing StorageModule from our state with the same storage module id
            let existing = local_modules_by_id
                .get(&sm_info.id)
                .ok_or_eyre("StorageModuleInfo must reference an existing storage module id")?;

            // Did this storage module from our state get assigned a new partition_hash ?
            if existing.partition_assignment().is_none() && sm_info.partition_assignment.is_some() {
                existing.assign_partition(sm_info.partition_assignment.unwrap(), update_height);

                // Record this storage module as needing packing, the protocol will always assign a new partition_hash
                // to capacity for 1 epoch so we can schedule this formerly unassigned storage module for packing
                packing_modules.push(existing.clone());

                // Skip any further validations for now
                continue;
            }

            // Get the path for this module - this is the only place the storage module id can be used as an index
            let path = &self.submodules_config.submodule_paths[sm_info.id];

            // Validate the path
            // ARCHITECTURE NOTE: Configuration vs. Implementation Mismatch
            //
            // There's a fundamental disconnect between the configuration system and the storage module design:
            //
            // 1. Original Design Intent:
            //    The StorageModule system was designed to support multiple submodules per StorageModule,
            //    allowing several smaller storage units to be combined into a single 16TB logical partition.
            //
            // 2. Current Configuration Limitation:
            //    The configuration system lacks the capability to express this many-to-one relationship.
            //
            // 3. Testnet Simplification:
            //    For Testnet, we adopt a simplified 1:1 mapping where each StorageModule contains
            //    exactly one submodule representing a full 16TB partition.
            //
            // This limitation should be addressed in future versions to fully realize the original
            // flexible storage architecture. see [`system_ledger::get_genesis_commitments()`] and
            // [`EpochServiceActor::map_storage_modules_to_partition_assignments`] for reference
            if *path != sm_info.submodules[0].1 {
                return Err(eyre::eyre!("Submodule paths don't match"));
            }

            // Validate the in memory storage module against on-disk packing parameters
            if let Some(info_pa) = sm_info.partition_assignment {
                // Validate the existing storage module info as it exists in our local state
                // vs. the existing packing params on disk to make sure everything is in sync
                // before updating the partition assignment
                match self.validate_packing_params(existing, path, sm_info.id) {
                    Ok(()) => {}
                    Err(err) => panic!("{}", err),
                }

                // Check to see if there's been a change in the ledger assignment for the partition_has
                // moved from Capacity->LedgerSlot or LedgerSlot->Capacity
                let existing_pa = existing.partition_assignment().unwrap();
                if info_pa.ledger_id != existing_pa.ledger_id
                    || info_pa.slot_index != existing_pa.slot_index
                {
                    let ledger_before = existing_pa.ledger_id;

                    // Update the storage modules partition assignment (and packing params toml)
                    // to match ledger/capacity reassignment
                    existing.assign_partition(info_pa, update_height);

                    if ledger_before.is_some() && info_pa.ledger_id.is_none() {
                        // This storage module is expiring from LedgerSlot->Capacity
                        packing_modules.push(existing.clone());
                    } else if ledger_before.is_none() && info_pa.ledger_id.is_some() {
                        // This storage module is assigned Capacity->LedgerSlot
                        assigned_modules.push(existing.clone());
                    }
                }
            }
        }

        // For each module requiring packing, start packing and mining
        for packing_sm in packing_modules {
            // Reset packing params and indexes on the storage module
            if let Ok(interval) = packing_sm.reset() {
                // Message packing service to fill up fresh entropy chunks on the drive
                let sender = self.service_senders.packing_sender();
                if let Ok(req) =
                    PackingRequest::new(packing_sm.clone(), PartitionChunkRange(interval))
                {
                    match sender.try_send(req) {
                        Ok(()) => {}
                        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                            tracing::warn!(
                                target = "irys::packing",
                                storage_module.id = %packing_sm.id,
                                storage_module.packing_interval = ?interval,
                                "Dropping packing request due to a saturated channel"
                            );
                        }
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_req)) => {
                            tracing::error!(
                                target = "irys::packing",
                                storage_module.id = %packing_sm.id,
                                storage_module.packing_interval = ?interval,
                                "Packing channel closed; failed to enqueue repacking request"
                            );
                        }
                    }
                }
            }
        }

        // For each storage module assigned to a data ledger, we need to update the data_root indexes
        // that may overlap the assigned slot so that it can index chunks
        let mut blocks_to_migrate: BTreeMap<u64, BlockHash> = BTreeMap::new();
        for assigned_sm in assigned_modules {
            // Rebuild indexes
            let ledger_id = assigned_sm
                .partition_assignment()
                .and_then(|a| a.ledger_id)
                .expect("storage module must be assigned to a data ledger slot");

            let ledger_range = assigned_sm
                .get_storage_module_ledger_offsets()
                .expect("storage module should be assigned to a ledger");

            // Get the chunk range in PartitionRelative offsets for the min and max
            // relative offset of data stored in this partition.
            let max_partition_offset = self.get_max_partition_offset(assigned_sm.clone());
            let partition_range = PartitionChunkOffset::from(0)..max_partition_offset;

            // Use a binary search to find the first chunk in that range without a tx_path_hash
            // We have to do this because we don't know if we're resuming this indexing from a crash
            // or restart of the node.
            let first_unindexed_chunk_offset =
                Self::find_first_unindexed_chunk(assigned_sm.clone(), partition_range);

            // If we found some unindexed chunks that should be present, let's build a list of blocks to migrate
            if let Some(chunk_offset) = first_unindexed_chunk_offset {
                let ledger_chunk_offset =
                    ledger_range.start() + LedgerChunkOffset::from(*chunk_offset);

                // check max_chunk_offset first
                let block_index_guard = self.block_index.read();
                let latest_item = match block_index_guard.get_latest_item() {
                    Some(item) => item,
                    None => continue, // No blocks yet, skip
                };

                let data_ledger = DataLedger::try_from(ledger_id).unwrap();
                let max_chunk_offset = latest_item.ledgers[data_ledger as usize].total_chunks;

                // Check if start offset is within bounds
                if *ledger_chunk_offset >= max_chunk_offset {
                    // This offset is beyond the actual data in the ledger, skip this storage module
                    continue;
                }

                // Now we can safely get block bounds for the start
                let block_bounds = block_index_guard
                    .get_block_bounds(data_ledger, ledger_chunk_offset)
                    .expect("Should be able to get block bounds as max_chunk_offset was checked");

                let start_block = block_bounds.height;

                // Calculate end offset
                let end_ledger_offset =
                    ledger_range.start() + LedgerChunkOffset::from(*max_partition_offset - 1);

                // Clamp end offset to actual data bounds
                let clamped_end_offset = if *end_ledger_offset >= max_chunk_offset {
                    if max_chunk_offset > 0 {
                        LedgerChunkOffset::from(max_chunk_offset - 1)
                    } else {
                        continue; // No data at all
                    }
                } else {
                    end_ledger_offset
                };

                // Ensure we have a valid range after clamping
                if clamped_end_offset < ledger_chunk_offset {
                    continue; // Skip if the range is invalid
                }

                // Now get block bounds for the end
                let end_block_bounds = block_index_guard
                    .get_block_bounds(data_ledger, clamped_end_offset)
                    .expect("Should be able to get end block bounds as offset was validated");

                let end_block = end_block_bounds.height;

                // Drop the guard before the next read
                let _ = block_index_guard;

                {
                    let bi = self.block_index.read();
                    for height in start_block..=end_block {
                        let block_hash = bi
                            .get_item(height)
                            .expect("block item to be present in index")
                            .block_hash;
                        blocks_to_migrate.insert(height, block_hash);
                    }
                }
            }
        }

        if !blocks_to_migrate.is_empty() {
            // If we have blocks to migrate, lets do so. The BTreeSet ensures in-order traversal
            let migration_service = &self.service_senders.chunk_migration;
            for (block_height, block_hash) in blocks_to_migrate {
                // Send migration request
                let (tx, rx) = tokio::sync::oneshot::channel();

                // Handle send error
                if let Err(e) = migration_service.send_traced(
                    ChunkMigrationServiceMessage::UpdateStorageModuleIndexes {
                        block_hash,
                        receiver: tx,
                    },
                ) {
                    error!(
                        "Failed to send migration request for block {} (height {}): {}",
                        block_hash, block_height, e
                    );
                    return Err(eyre!(
                        "Unable to index storage module chunks do to mpsc send failure: {}",
                        e
                    ));
                }

                // We await responses so we only perform one migration at a time
                if let Err(e) = rx.await {
                    error!(
                        "Failed to receive migration response for block {} (height {}): {}",
                        block_hash, block_height, e
                    );
                }
            }
        }

        // Only once the storage module partition assignments are updated we can a
        // safely update the data_sync_service for any necessary data synchronization
        if let Err(e) = self
            .service_senders
            .data_sync
            .send_traced(DataSyncServiceMessage::SyncPartitions)
        {
            error!(
                "Failed to send SyncPartitions message to data_sync service: {}",
                e
            );
        }

        Ok(())
    }

    #[tracing::instrument(level = "trace", skip_all)]
    fn clear_assignment_if_outdated(
        &self,
        module: &Arc<StorageModule>,
        update_height: u64,
        reason: &'static str,
    ) {
        if module.partition_assignment().is_none() {
            return;
        }

        let path = &self.submodules_config.submodule_paths[module.id];
        let params_path = path.join(PACKING_PARAMS_FILE_NAME);
        let newer_local = match PackingParams::from_toml(&params_path) {
            Ok(params) => params
                .last_updated_height
                .is_some_and(|h| h > update_height),
            Err(_) => false,
        };

        if newer_local {
            debug!(
                storage_module.id = module.id,
                storage_module.update_height = update_height,
                storage_module.clear_reason = reason,
                "skipping unassign: local packing params are newer than update"
            );
            return;
        }

        debug!(
            storage_module.id = module.id,
            storage_module.update_height = update_height,
            storage_module.clear_reason = reason,
            "clearing local partition assignment"
        );
        module.clear_assignment(update_height);

        match module.reset() {
            Ok(interval) => {
                debug!(
                    packing.interval = ?interval,
                    storage_module.id = module.id,
                    storage_module.clear_reason = reason,
                    "storage module reset after unassign"
                );
            }
            Err(e) => {
                warn!(
                    storage_module.id = module.id,
                    storage_module.clear_reason = reason,
                    "failed to reset storage module after unassign: {}",
                    e
                );
            }
        }
    }

    /// Gets the maximum offset in the partition that has been assigned data chunks
    /// by data transactions. Used for retrieving the  the maximum possible ledger offset
    /// within a partition relative to the start of the partition.
    fn get_max_partition_offset(&self, storage_module: Arc<StorageModule>) -> PartitionChunkOffset {
        let ledger_id = storage_module
            .partition_assignment()
            .and_then(|a| a.ledger_id)
            .expect("storage module must be assigned to a data ledger slot");

        let current_height = self.block_tree.read().get_latest_canonical_entry().height();
        let migration_height =
            current_height.saturating_sub(self.config.consensus.block_migration_depth as u64);

        let max_ledger_offset = self
            .block_tree
            .get_total_chunks(migration_height, ledger_id);

        let range = storage_module
            .get_storage_module_ledger_offsets()
            .expect("storage module should be assigned to a ledger");
        let start: u64 = *range.start();
        let end: u64 = *range.end();

        // Make the max offset partition relative
        let part_end = match max_ledger_offset {
            Some(max) if end >= *max => max.saturating_sub(start),
            Some(_) => end - start,
            None => 0,
        };

        PartitionChunkOffset::from(part_end)
    }

    fn find_first_unindexed_chunk(
        storage_module: Arc<StorageModule>,
        range: Range<PartitionChunkOffset>,
    ) -> Option<PartitionChunkOffset> {
        let mut lo = range.start;
        let mut hi = range.end;

        let mut path_hashes: Option<ChunkPathHashes> = None;

        while lo < hi {
            let mid = lo + (hi - lo) / 2;

            path_hashes = storage_module
                .query_submodule_db_by_offset(mid, |tx| get_path_hashes_by_offset(tx, mid))
                .expect("to be able to query submodule db");

            if path_hashes.is_some() {
                lo = mid + 1; // skip initialized
            } else {
                hi = mid; // candidate for first uninitialized
            }
        }

        if lo < range.end && path_hashes.is_none() {
            Some(lo)
        } else {
            None
        }
    }

    /// Validates that a storage module's partition assignment matches the on-disk parameters.
    /// Reports an error if there's a mismatch.
    #[tracing::instrument(level = "trace", skip_all, err)]
    fn validate_packing_params(
        &self,
        module: &StorageModule,
        module_path: &Path,
        index: usize,
    ) -> eyre::Result<()> {
        // Skip modules without partition assignments
        if module.partition_assignment().is_none() {
            warn!(
                "Storage module {:?} at index {} has no partition assignment",
                &module_path, index
            );
            return Ok(());
        }

        // Get the assignment
        let assignment = module.partition_assignment().unwrap();

        // Load parameters from disk
        let params_path = module_path.join("packing_params.toml");
        let params = match PackingParams::from_toml(&params_path) {
            Ok(p) => p,
            Err(e) => {
                warn!(
                    "Failed to load packing params for module {:?} at index {}: {}",
                    &module_path, index, e
                );
                return Ok(()); // Skip validation
            }
        };

        // Check all parameters
        let hash_match = assignment.partition_hash == params.partition_hash.unwrap();
        let slot_match = assignment.slot_index == params.slot;
        let ledger_match = assignment.ledger_id == params.ledger;

        // Report overall status
        if hash_match && slot_match && ledger_match {
            debug!(
                "Storage module {:?} at index {} matches on-disk parameters",
                &module_path, index
            );
            return Ok(());
        }

        // Collect detailed mismatch information for error message
        let mut mismatches = Vec::new();

        if !hash_match {
            mismatches.push(format!(
                "partition hash: module={:?}, disk={:?}",
                assignment.partition_hash, params.partition_hash
            ));
        }

        if !slot_match {
            mismatches.push(format!(
                "slot index: module={:?}, disk={:?}",
                assignment.slot_index, params.slot
            ));
        }

        if !ledger_match {
            mismatches.push(format!(
                "ledger ID: module={:?}, disk={:?}",
                assignment.ledger_id, params.ledger
            ));
        }

        // Return a detailed error with all mismatches
        Err(eyre::eyre!(
            "Storage module {:?} at index {} has mismatched parameters: {}",
            &module_path,
            index,
            mismatches.join(", ")
        ))
    }
}

/// mpsc style service wrapper for the Storage Module Service
impl StorageModuleService {
    /// Spawn a new StorageModule service
    #[tracing::instrument(level = "trace", skip_all, name = "spawn_service_storage_module")]
    pub fn spawn_service(
        rx: UnboundedReceiver<Traced<StorageModuleServiceMessage>>,
        storage_modules: Arc<RwLock<Vec<Arc<StorageModule>>>>,
        block_index: BlockIndexReadGuard,
        block_tree: BlockTreeReadGuard,
        service_senders: ServiceSenders,
        config: &Config,
        runtime_handle: tokio::runtime::Handle,
    ) -> TokioServiceHandle {
        tracing::info!("Spawning storage module service");

        let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();

        let config = config.clone();

        let handle = runtime_handle.spawn(
            async move {
                let pending_storage_module_service = Self {
                    shutdown: shutdown_rx,
                    msg_rx: rx,
                    inner: StorageModuleServiceInner::new(
                        storage_modules,
                        block_index,
                        block_tree,
                        service_senders,
                        config,
                    ),
                };
                pending_storage_module_service
                    .start()
                    .await
                    .expect("StorageModule Service encountered an irrecoverable error")
            }
            .instrument(tracing::Span::current()),
        );

        TokioServiceHandle {
            name: "storage_module_service".to_string(),
            handle,
            shutdown_signal: shutdown_tx,
        }
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    async fn start(mut self) -> eyre::Result<()> {
        tracing::info!("starting StorageModule Service");

        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.tick().await; // Skip first immediate tick

        loop {
            tokio::select! {
                biased;

                // Check for shutdown signal
                _ = &mut self.shutdown => {
                    tracing::info!("Shutdown signal received for storage module service");
                    break;
                }
                // Handle messages
                msg = self.msg_rx.recv() => {
                    match msg {
                        Some(traced) => {
                            let (msg, parent_span) = traced.into_parts();
                            let span = tracing::trace_span!(parent: &parent_span, "storage_module_handle_message");
                            self.inner.handle_message(msg).instrument(span).await?;
                        }
                        None => {
                            tracing::warn!("Message channel closed unexpectedly");
                            break;
                        }
                    }
                }
                // Handle ticks of the interval
                _ = interval.tick() => {
                     self.inner.tick();
                }
            }
        }

        tracing::debug!(custom.amount_of_messages = ?self.msg_rx.len(), "processing last in-bound messages before shutdown");
        while let Ok(traced) = self.msg_rx.try_recv() {
            let (msg, parent_span) = traced.into_parts();
            let span = tracing::trace_span!(parent: &parent_span, "storage_module_handle_message");
            self.inner.handle_message(msg).instrument(span).await?
        }

        tracing::info!("shutting down StorageModule Service gracefully");
        Ok(())
    }
}
