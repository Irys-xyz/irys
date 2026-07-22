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
    DataSyncServiceMessage, chunk_migration_service::ChunkMigrationServiceMessage,
    packing_service::PackingRequest, services::ServiceSenders,
};
use eyre::{OptionExt as _, eyre};
use irys_config::StorageSubmodulesConfig;
use irys_domain::{
    BlockBoundsError, BlockIndexReadGuard, BlockTreeReadGuard, PACKING_PARAMS_FILE_NAME,
    PackingParams, StorageModule, StorageModuleInfo,
};
use irys_types::{
    BlockHash, Config, DataLedger, LedgerChunkOffset, PartitionChunkOffset, PartitionChunkRange,
    SendTraced as _, TokioServiceHandle, Traced,
};
use reth::tasks::shutdown::Shutdown;
use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::sync::{mpsc::UnboundedReceiver /*, oneshot*/};
use tracing::{Instrument as _, debug, error, warn};

/// Max blocks re-indexed in one heal pass. Remaining path-hash holes retry on
/// the next assignment update / restart so a first-gap→frontier repair cannot
/// monopolize startup or the assignment path for unbounded wall time.
const INDEX_HEAL_MAX_BLOCKS_PER_PASS: usize = 128;

/// Per-block wait for `UpdateStorageModuleIndexes`. If migration neither
/// responds nor drops the oneshot, heal soft-skips and continues.
const INDEX_HEAL_MIGRATE_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

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
        let submodules_config = match StorageSubmodulesConfig::load(
            config.node_config.base_directory.clone(),
            config.node_config.node_mode,
        ) {
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
            if sm.last_pending_write().elapsed() > Duration::from_secs(5)
                && let Err(e) = sm.force_sync_pending_chunks()
            {
                error!(
                    "Couldn't flush pending chunks for storage_module {}: {}",
                    sm.id, e
                );
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
                    info.id,
                    info
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
            if existing.partition_assignment().is_none()
                && let Some(assignment) = sm_info.partition_assignment
            {
                existing.assign_partition(assignment, update_height);

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
                    }
                    // Capacity→LedgerSlot and long-lived ledger assigns: index
                    // repair runs below for *all* ledger-assigned SMs (path-hash
                    // gap scan). Direct ledger A→B reassignment relies on the
                    // mining-bus reset path, not this presence scan.
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

        // After assignments settle: gap-scan + index backfill for every local SM
        // currently on a data ledger (not only Capacity→LedgerSlot transitions).
        self.heal_ledger_data_indexes().await?;

        Ok(())
    }

    /// Scan ledger-assigned SMs for path-hash index gaps, backfill via
    /// `UpdateStorageModuleIndexes`, then `SyncPartitions` (membership always;
    /// unblock only if plan + migrate reported no issues).
    ///
    /// Startup and every partition-assignment update. Dense indexes → density
    /// fast path / no migrate. Soft-skips bounds/index inconsistency and per-block
    /// migration errors (including response timeouts); hard-fails only if the
    /// migration channel is closed. Large repairs are chunked
    /// ([`INDEX_HEAL_MAX_BLOCKS_PER_PASS`]) so startup cannot hang unbounded.
    ///
    /// Marker is path-hash completeness; `UpdateStorageModuleIndexes` also writes
    /// DataRootInfos when it runs. Residual: dense path-hashes with empty
    /// DataRootInfos will not schedule a migrate.
    #[tracing::instrument(level = "debug", skip_all, err)]
    async fn heal_ledger_data_indexes(&self) -> eyre::Result<()> {
        let ledger_modules: Vec<Arc<StorageModule>> = {
            let guard = self.storage_modules.read().unwrap();
            guard
                .iter()
                .filter(|sm| {
                    sm.partition_assignment()
                        .and_then(|a| a.ledger_id)
                        .is_some()
                })
                .cloned()
                .collect()
        };

        debug!(
            storage_module.ledger_assigned_count = ledger_modules.len(),
            "healing data indexes for ledger-assigned storage modules"
        );

        let mut blocks_to_migrate: BTreeMap<u64, BlockHash> = BTreeMap::new();
        let mut heal_issues = 0_usize;
        for sm in &ledger_modules {
            match self.plan_index_repair(sm) {
                IndexRepairPlan::Complete => {}
                IndexRepairPlan::SoftSkipped => {
                    heal_issues += 1;
                }
                IndexRepairPlan::NeedsRepair { height_spans } => {
                    let bi = self.block_index.read();
                    let mut planned_any = false;
                    let mut missing_height = false;
                    'spans: for (start_height, end_height) in height_spans {
                        for height in start_height..=end_height {
                            let Some(item) = bi.get_item(height) else {
                                // Index shrank / truncated between plan and read (e.g. reorg).
                                warn!(
                                    storage_module.id = sm.id,
                                    block.height = height,
                                    plan.start_height = start_height,
                                    plan.end_height = end_height,
                                    "block missing from index during heal; counting as heal issue"
                                );
                                missing_height = true;
                                break 'spans;
                            };
                            blocks_to_migrate.insert(height, item.block_hash);
                            planned_any = true;
                        }
                    }
                    // Incomplete materialization (empty span or mid-range gap) —
                    // count once, not on both the miss and the empty check.
                    if missing_height || !planned_any {
                        heal_issues += 1;
                    }
                }
            }
        }

        if !blocks_to_migrate.is_empty() {
            debug!(
                index_heal.blocks = blocks_to_migrate.len(),
                "migrating blocks to repair storage-module data indexes"
            );
            heal_issues += self
                .migrate_storage_module_indexes(blocks_to_migrate)
                .await?;
        }

        // Always refresh orchestrator membership. Unblock only when every SM plan
        // was Complete or fully migrated — plan soft-skips and migrate failures
        // both suppress unblock to avoid epoch refetch/re-block thrash.
        let unblock = heal_issues == 0;
        if !unblock {
            warn!(
                index_heal.issues = heal_issues,
                "index heal incomplete; SyncPartitions without unblock \
                 to avoid refetch/re-block thrash"
            );
        }

        if let Err(e) =
            self.service_senders
                .data_sync
                .send_traced(DataSyncServiceMessage::SyncPartitions {
                    unblock_missing_data_root_index: unblock,
                })
        {
            error!(
                "Failed to send SyncPartitions message to data_sync service: {}",
                e
            );
        }

        Ok(())
    }

    /// Path-hash holes → inclusive block-height spans to re-index (one span per
    /// hole), or a soft outcome.
    ///
    /// Each hole maps to the minimal block-height range that covers it — not the
    /// full SM/frontier tail after the first gap (avoids redundant re-migration of
    /// already-dense segments between disjoint holes).
    fn plan_index_repair(&self, sm: &Arc<StorageModule>) -> IndexRepairPlan {
        let ledger_id = sm
            .partition_assignment()
            .and_then(|a| a.ledger_id)
            .expect("storage module must be assigned to a data ledger slot");

        let Ok(ledger_range) = sm.get_storage_module_ledger_offsets() else {
            warn!(
                storage_module.id = sm.id,
                "storage module not assigned to a ledger during index plan; soft-skip"
            );
            return IndexRepairPlan::SoftSkipped;
        };

        let max_partition_offset = self.get_max_partition_offset(sm.clone());
        if *max_partition_offset == 0 {
            // No migrated data in range yet — nothing to repair.
            return IndexRepairPlan::Complete;
        }

        // All path-hash holes in the SM scan window (density fast path or walk).
        let gaps = match sm
            .missing_path_hash_ranges(PartitionChunkOffset::from(0), max_partition_offset)
        {
            Ok(gaps) if gaps.is_empty() => return IndexRepairPlan::Complete,
            Ok(gaps) => gaps,
            Err(e) => {
                warn!(
                    storage_module.id = sm.id,
                    error = %e,
                    "path-hash gap scan failed; soft-skipping index repair for this module"
                );
                return IndexRepairPlan::SoftSkipped;
            }
        };

        debug!(
            storage_module.id = sm.id,
            index_heal.gap_count = gaps.len(),
            index_heal.first_gap = %gaps[0].0,
            index_heal.max_partition_offset = %max_partition_offset,
            "path-hash index gaps detected; scheduling per-hole index repair"
        );

        let block_index_guard = self.block_index.read();
        let Some(latest_item) = block_index_guard.get_latest_item() else {
            // Gaps exist but chain index is empty — cannot repair yet.
            return IndexRepairPlan::SoftSkipped;
        };

        let Ok(data_ledger) = DataLedger::try_from(ledger_id) else {
            warn!(
                storage_module.id = sm.id,
                ledger_id, "invalid ledger id during index plan; soft-skip"
            );
            return IndexRepairPlan::SoftSkipped;
        };
        // Frontier for this ledger; absent entry (e.g. term ledger pre-Cascade)
        // means no data yet — still needed to clamp hole ends below.
        let max_chunk_offset = latest_item
            .ledgers
            .iter()
            .find(|l| l.ledger == data_ledger)
            .map(|l| l.total_chunks)
            .unwrap_or(0);

        if max_chunk_offset == 0 {
            return IndexRepairPlan::Complete;
        }

        let mut height_spans = Vec::new();
        let mut any_soft_skip = false;
        for (gap_start, gap_end) in gaps {
            // Half-open [gap_start, gap_end) → inclusive last missing offset.
            if gap_start >= gap_end {
                continue;
            }
            let gap_last = PartitionChunkOffset(*gap_end - 1);

            let start_ledger = ledger_range.start() + LedgerChunkOffset::from(*gap_start);
            if *start_ledger >= max_chunk_offset {
                // Entire hole is past the ledger frontier.
                continue;
            }

            let end_ledger = ledger_range.start() + LedgerChunkOffset::from(*gap_last);
            let clamped_end = if *end_ledger >= max_chunk_offset {
                LedgerChunkOffset::from(max_chunk_offset - 1)
            } else {
                end_ledger
            };
            if clamped_end < start_ledger {
                continue;
            }

            let Some(start_block) = Self::block_height_for_ledger_offset(
                block_index_guard,
                data_ledger,
                start_ledger,
                sm.id,
                "gap_start",
            ) else {
                any_soft_skip = true;
                continue;
            };
            let Some(end_block) = Self::block_height_for_ledger_offset(
                block_index_guard,
                data_ledger,
                clamped_end,
                sm.id,
                "gap_end",
            ) else {
                any_soft_skip = true;
                continue;
            };

            height_spans.push((start_block, end_block));
        }

        if height_spans.is_empty() {
            // All holes past frontier → complete; unresolved bounds → soft-skip.
            return if any_soft_skip {
                IndexRepairPlan::SoftSkipped
            } else {
                IndexRepairPlan::Complete
            };
        }

        // Prefer reporting SoftSkipped if some holes could not be planned, so we
        // do not claim a full successful heal / unblock.
        if any_soft_skip {
            return IndexRepairPlan::SoftSkipped;
        }

        IndexRepairPlan::NeedsRepair { height_spans }
    }

    /// Map a ledger chunk offset to a block height, or soft-skip on bounds errors.
    fn block_height_for_ledger_offset(
        block_index: &irys_domain::BlockIndex,
        data_ledger: DataLedger,
        offset: LedgerChunkOffset,
        storage_module_id: usize,
        bound_label: &'static str,
    ) -> Option<u64> {
        match block_index.get_block_bounds(data_ledger, offset) {
            Ok(bounds) => Some(bounds.height),
            Err(
                BlockBoundsError::IndexEmpty
                | BlockBoundsError::LedgerInactive { .. }
                | BlockBoundsError::OffsetBeyondFrontier { .. },
            ) => None,
            Err(BlockBoundsError::Internal(e)) => {
                warn!(
                    storage_module.id = storage_module_id,
                    error = %e,
                    bound = bound_label,
                    "block bounds internal error; soft-skipping index repair"
                );
                None
            }
        }
    }

    /// Sequential `UpdateStorageModuleIndexes` for each block (BTreeMap order).
    ///
    /// Caps work at [`INDEX_HEAL_MAX_BLOCKS_PER_PASS`] and awaits each response
    /// with [`INDEX_HEAL_MIGRATE_RESPONSE_TIMEOUT`] so startup / assignment heal
    /// cannot hang indefinitely if migration stalls. Deferred and failed blocks
    /// count toward the returned failure total (suppresses unblock).
    ///
    /// Hard-fails only if the migration mpsc is closed.
    async fn migrate_storage_module_indexes(
        &self,
        blocks_to_migrate: BTreeMap<u64, BlockHash>,
    ) -> eyre::Result<usize> {
        let total_planned = blocks_to_migrate.len();
        let mut failures = 0_usize;

        let blocks_this_pass: BTreeMap<u64, BlockHash> =
            if total_planned > INDEX_HEAL_MAX_BLOCKS_PER_PASS {
                let deferred = total_planned - INDEX_HEAL_MAX_BLOCKS_PER_PASS;
                warn!(
                    index_heal.planned = total_planned,
                    index_heal.pass_limit = INDEX_HEAL_MAX_BLOCKS_PER_PASS,
                    index_heal.deferred = deferred,
                    "capping index heal migrate pass; remaining holes retry on next heal"
                );
                // Count deferred as incomplete heal so we do not mass-unblock early.
                failures += deferred;
                blocks_to_migrate
                    .into_iter()
                    .take(INDEX_HEAL_MAX_BLOCKS_PER_PASS)
                    .collect()
            } else {
                blocks_to_migrate
            };

        let migration_service = &self.service_senders.chunk_migration;
        for (block_height, block_hash) in blocks_this_pass {
            let (tx, rx) = tokio::sync::oneshot::channel();

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
                    "Unable to index storage module chunks due to mpsc send failure: {}",
                    e
                ));
            }

            match tokio::time::timeout(INDEX_HEAL_MIGRATE_RESPONSE_TIMEOUT, rx).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(e))) => {
                    warn!(
                        block.hash = %block_hash,
                        block.height = block_height,
                        error = %e,
                        "UpdateStorageModuleIndexes failed; soft-skipping block"
                    );
                    failures += 1;
                }
                Ok(Err(e)) => {
                    error!(
                        "Failed to receive migration response for block {} (height {}): {}",
                        block_hash, block_height, e
                    );
                    failures += 1;
                }
                Err(_elapsed) => {
                    warn!(
                        block.hash = %block_hash,
                        block.height = block_height,
                        timeout_secs = INDEX_HEAL_MIGRATE_RESPONSE_TIMEOUT.as_secs(),
                        "UpdateStorageModuleIndexes response timed out; soft-skipping block"
                    );
                    failures += 1;
                }
            }
        }
        Ok(failures)
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

    /// Exclusive end of the partition-relative offset range that may hold migrated
    /// data (`0..excl` covers partition offsets `0..=excl-1` when `excl > 0`).
    ///
    /// `get_storage_module_ledger_offsets` builds an `ie` interval, which nodit
    /// stores as **inclusive** start/end. `total_chunks` is an **exclusive**
    /// ledger frontier. Mixing those without +1 on the full-SM arm drops the
    /// last chunk from the scan / backfill window.
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
        // Inclusive bounds after nodit `ie` → stored inclusive end.
        let start: u64 = *range.start();
        let end_incl: u64 = *range.end();
        let max_excl = max_ledger_offset.map(|m| *m);

        PartitionChunkOffset::from(exclusive_partition_end(start, end_incl, max_excl))
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

/// Outcome of planning path-hash index repair for one ledger-assigned SM.
enum IndexRepairPlan {
    /// Path-hash scan found no gap (or no data in range).
    Complete,
    /// Inclusive block-height spans covering path-hash holes only (not full tail).
    NeedsRepair { height_spans: Vec<(u64, u64)> },
    /// Gap or bounds uncertainty; heal must not claim success / unblock.
    SoftSkipped,
}

/// Exclusive partition-relative end for index scan / backfill.
///
/// - `start` / `end_incl`: SM ledger span as **inclusive** bounds (nodit `ie` storage)
/// - `max_ledger_excl`: ledger `total_chunks` frontier (**exclusive**), if known
///
/// Returns `N` such that partition offsets `0..N` should be considered for indexing.
fn exclusive_partition_end(start: u64, end_incl: u64, max_ledger_excl: Option<u64>) -> u64 {
    let sm_len_excl = end_incl.saturating_sub(start).saturating_add(1);
    match max_ledger_excl {
        // Exclusive frontier still inside the SM (including exactly past last inclusive).
        Some(max) if max > start && max <= end_incl.saturating_add(1) => max.saturating_sub(start),
        // Frontier at/before SM start → nothing to index.
        Some(max) if max <= start => 0,
        // Frontier past the SM → full module span.
        Some(_) => sm_len_excl,
        None => 0,
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

    #[tracing::instrument(name = "storage_module_service_start", level = "trace", skip_all, err)]
    async fn start(mut self) -> eyre::Result<()> {
        tracing::info!("starting StorageModule Service");

        // Crash-resume / long-lived ledger assigns never see Capacity→LedgerSlot
        // again. Soft-skips per-SM issues; only migration channel closed is fatal.
        // Migrate is chunked + per-response timeout so first-gap→frontier repair
        // cannot block this start path indefinitely (see migrate_storage_module_indexes).
        if let Err(e) = self.inner.heal_ledger_data_indexes().await {
            error!("startup data-index heal failed: {e:?}");
            return Err(e);
        }

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

#[cfg(test)]
mod exclusive_partition_end_tests {
    use super::exclusive_partition_end;

    #[test]
    fn full_sm_includes_last_chunk_when_frontier_past_sm() {
        // SM ledger [0, 99] inclusive (nodit ie of [0, 100)).
        // Old arm used end - start = 99 as exclusive end → dropped offset 99.
        assert_eq!(exclusive_partition_end(0, 99, Some(1_000)), 100);
        assert_eq!(exclusive_partition_end(0, 99, Some(100)), 100);
    }

    #[test]
    fn partial_frontier_inside_sm_is_exclusive_relative() {
        // total_chunks exclusive 50 → offsets 0..49.
        assert_eq!(exclusive_partition_end(0, 99, Some(50)), 50);
    }

    #[test]
    fn frontier_before_sm_is_empty() {
        assert_eq!(exclusive_partition_end(100, 199, Some(50)), 0);
        assert_eq!(exclusive_partition_end(100, 199, Some(100)), 0);
    }

    #[test]
    fn frontier_none_is_empty() {
        assert_eq!(exclusive_partition_end(0, 99, None), 0);
    }

    #[test]
    fn non_zero_sm_start() {
        // SM [100, 199] incl, frontier 150 excl → relative exclusive end 50.
        assert_eq!(exclusive_partition_end(100, 199, Some(150)), 50);
        // Frontier past SM → full length 100.
        assert_eq!(exclusive_partition_end(100, 199, Some(500)), 100);
    }
}
