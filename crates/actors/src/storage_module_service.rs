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
use futures::FutureExt as _;
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

    async fn handle_message(
        &mut self,
        msg: StorageModuleServiceMessage,
        cancel: Option<&Shutdown>,
    ) -> eyre::Result<()> {
        match msg {
            StorageModuleServiceMessage::PartitionAssignmentsUpdated {
                storage_module_infos,
                update_height,
            } => {
                self.handle_partition_assignments_update(
                    storage_module_infos,
                    update_height,
                    cancel,
                )
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
        cancel: Option<&Shutdown>,
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
        // `cancel` is the run loop's shutdown clone, so a long epoch backfill bails
        // between blocks on shutdown, same as the startup heal.
        self.heal_ledger_data_indexes(cancel).await?;

        Ok(())
    }

    /// Per-SM path-hash index heal: gap-scan each ledger-assigned SM, backfill the
    /// covering blocks via `UpdateStorageModuleIndexes`, then `SyncPartitions`
    /// (membership always; unblock only SMs confirmed gap-free this pass). Runs at
    /// startup and every partition-assignment update; dense indexes take the
    /// density fast path (no migrate). Soft-skips bounds/index inconsistency;
    /// hard-fails only if the migration channel is closed.
    ///
    /// Cleanliness is per SM by a post-migration re-verify, not migrate success:
    /// a migrate that returns `Ok` without closing the gap (reorg orphan-skip /
    /// TOCTOU) leaves a real gap, so re-verify withholds that SM's unblock while
    /// healthy SMs still unblock. Per-block migration errors (including response
    /// timeouts) count as failures; large repairs are chunked
    /// ([`INDEX_HEAL_MAX_BLOCKS_PER_PASS`]) so startup cannot hang unbounded.
    ///
    /// Marker is path-hash completeness (`UpdateStorageModuleIndexes` writes
    /// DataRootInfos alongside). Residual: dense path-hashes with empty
    /// DataRootInfos won't schedule a migrate.
    ///
    /// `cancel` (a cheap `Shutdown` clone) lets a long per-block migration bail
    /// within ~one block when shutdown is requested mid-heal.
    #[tracing::instrument(level = "debug", skip_all, err)]
    async fn heal_ledger_data_indexes(&self, cancel: Option<&Shutdown>) -> eyre::Result<()> {
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

        let mut clean_sm_ids: Vec<usize> = Vec::new();
        // (SM, plan-time exclusive partition-offset bound) pairs to re-verify
        // after migration, against the snapshot range — never a re-read frontier.
        let mut needs_reverify: Vec<(Arc<StorageModule>, PartitionChunkOffset)> = Vec::new();
        let mut blocks_to_migrate: BTreeMap<u64, BlockHash> = BTreeMap::new();

        for sm in &ledger_modules {
            match self.plan_index_repair(sm) {
                // No gap at plan time, so no migrate. Still re-verified below
                // (against the dense-prefix bound) so a reorg-recovery clear that
                // races the plan→send window is caught and the SM is withheld
                // rather than falsely unblocked.
                IndexRepairPlan::Complete { reverify_max } => {
                    needs_reverify.push((sm.clone(), reverify_max));
                }
                // Plan uncertain → not clean; leave blocked and signal operators.
                IndexRepairPlan::SoftSkipped => {
                    crate::metrics::record_index_heal_unrepaired("soft_skip");
                }
                IndexRepairPlan::NeedsRepair {
                    start_height,
                    end_height,
                    max_partition_offset,
                } => {
                    // Best-effort materialization: a height missing from the index
                    // (e.g. reorg truncation) just means the gap may remain — the
                    // post-migration re-verify is the source of truth, so we don't
                    // count per-block failures here.
                    let bi = self.block_index.read();
                    for height in start_height..=end_height {
                        let Some(item) = bi.get_item(height) else {
                            warn!(
                                storage_module.id = sm.id,
                                block.height = height,
                                plan.start_height = start_height,
                                plan.end_height = end_height,
                                "block missing from index during heal; re-verify will decide cleanliness"
                            );
                            break;
                        };
                        blocks_to_migrate.insert(height, item.block_hash);
                    }
                    needs_reverify.push((sm.clone(), max_partition_offset));
                }
            }
        }

        if !blocks_to_migrate.is_empty() {
            debug!(
                index_heal.blocks = blocks_to_migrate.len(),
                "migrating blocks to repair storage-module data indexes"
            );
            // Hard-fail only on migration-channel closed (propagated). The
            // per-block failure count is intentionally ignored — re-verify below
            // decides which SMs are actually clean.
            let _ = self
                .migrate_storage_module_indexes(blocks_to_migrate, cancel)
                .await?;
        }

        // Post-migration re-verify: an SM that needed repair is clean iff the
        // range it *planned* to repair now has no path-hash gap. Scanning the
        // plan-time `[0, max_plan)` (not a freshly-read frontier) means a chain
        // advancing during the heal cannot manufacture a false gap past what this
        // pass set out to fix. Orphan-skip / TOCTOU / missing-data all leave a
        // real gap in this range and correctly withhold the SM's unblock.
        for (sm, max_plan) in &needs_reverify {
            match sm.first_missing_path_hash_offset(PartitionChunkOffset::from(0), *max_plan) {
                Ok(None) => clean_sm_ids.push(sm.id),
                Ok(Some(gap)) => {
                    warn!(
                        storage_module.id = sm.id,
                        index_heal.first_gap = %gap,
                        index_heal.max_partition_offset = %max_plan,
                        "path-hash index still gapped after heal; withholding unblock"
                    );
                    crate::metrics::record_index_heal_unrepaired("gap_after_heal");
                }
                Err(e) => {
                    warn!(
                        storage_module.id = sm.id,
                        error = %e,
                        "path-hash re-verify scan failed after heal; withholding unblock"
                    );
                    crate::metrics::record_index_heal_unrepaired("soft_skip");
                }
            }
        }

        // Always refresh orchestrator membership; unblock only confirmed-clean SMs.
        if let Err(e) =
            self.service_senders
                .data_sync
                .send_traced(DataSyncServiceMessage::SyncPartitions {
                    unblock_sm_ids: clean_sm_ids,
                })
        {
            error!(
                "Failed to send SyncPartitions message to data_sync service: {}",
                e
            );
        }

        Ok(())
    }

    /// Path-hash gap → inclusive block-height range to re-index, or a soft outcome.
    ///
    /// Distinguishes "indexes look complete" from "could not plan / uncertain" so
    /// heal does not claim success and mass-unblock after a soft-skip.
    fn plan_index_repair(&self, sm: &Arc<StorageModule>) -> IndexRepairPlan {
        let Some(ledger_id) = sm.partition_assignment().and_then(|a| a.ledger_id) else {
            warn!(
                storage_module.id = sm.id,
                "storage module not assigned to a data ledger slot during index plan; soft-skip"
            );
            return IndexRepairPlan::SoftSkipped;
        };

        let Ok(ledger_range) = sm.get_storage_module_ledger_offsets() else {
            warn!(
                storage_module.id = sm.id,
                "storage module not assigned to a ledger during index plan; soft-skip"
            );
            return IndexRepairPlan::SoftSkipped;
        };

        let Some(max_partition_offset) = self.get_max_partition_offset(sm) else {
            // Cannot determine bounds → uncertain, soft-skip.
            return IndexRepairPlan::SoftSkipped;
        };
        if *max_partition_offset == 0 {
            // No migrated data in range yet — nothing to repair or re-verify.
            return IndexRepairPlan::Complete {
                reverify_max: PartitionChunkOffset::from(0),
            };
        }

        // First offset without path hashes (density fast path or cursor walk).
        let chunk_offset = match sm
            .first_missing_path_hash_offset(PartitionChunkOffset::from(0), max_partition_offset)
        {
            Ok(Some(offset)) => offset,
            // Fully dense over [0, max) — re-verify the whole range.
            Ok(None) => {
                return IndexRepairPlan::Complete {
                    reverify_max: max_partition_offset,
                };
            }
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
            index_heal.first_gap = %chunk_offset,
            index_heal.max_partition_offset = %max_partition_offset,
            "path-hash index gap detected; scheduling index repair"
        );

        let ledger_chunk_offset = ledger_range.start() + LedgerChunkOffset::from(*chunk_offset);
        let block_index_guard = self.block_index.read();
        let Some(latest_item) = block_index_guard.get_latest_item() else {
            // Gap exists but chain index is empty — cannot repair yet.
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
        // means no data yet — still needed to clamp the end offset below.
        let max_chunk_offset = latest_item
            .ledgers
            .iter()
            .find(|l| l.ledger == data_ledger)
            .map(|l| l.total_chunks)
            .unwrap_or(0);

        if *ledger_chunk_offset >= max_chunk_offset {
            // Gap is past the ledger frontier — not repairable as missing index.
            // The dense prefix [0, chunk_offset) is what to re-verify.
            return IndexRepairPlan::Complete {
                reverify_max: chunk_offset,
            };
        }

        let Some(start_block) = Self::block_height_for_ledger_offset(
            block_index_guard,
            data_ledger,
            ledger_chunk_offset,
            sm.id,
            "start",
        ) else {
            return IndexRepairPlan::SoftSkipped;
        };

        // Inclusive last ledger offset: max_partition_offset is exclusive.
        let end_ledger_offset =
            ledger_range.start() + LedgerChunkOffset::from(*max_partition_offset - 1);
        let clamped_end_offset = if *end_ledger_offset >= max_chunk_offset {
            if max_chunk_offset == 0 {
                return IndexRepairPlan::Complete {
                    reverify_max: chunk_offset,
                };
            }
            LedgerChunkOffset::from(max_chunk_offset - 1)
        } else {
            end_ledger_offset
        };

        if clamped_end_offset < ledger_chunk_offset {
            return IndexRepairPlan::SoftSkipped;
        }

        let Some(end_block) = Self::block_height_for_ledger_offset(
            block_index_guard,
            data_ledger,
            clamped_end_offset,
            sm.id,
            "end",
        ) else {
            return IndexRepairPlan::SoftSkipped;
        };

        // Snapshot the re-verify bound from the block-index frontier the plan
        // clamped to (`clamped_end_offset`), not the looser block-tree-derived
        // `max_partition_offset`. When migration lags, the block-tree frontier
        // (tip − depth) overshoots what is locally repairable; re-verifying to
        // that bound would see a phantom tail gap and needlessly withhold this
        // SM's unblock for one cycle. `clamped_end_offset >= ledger_chunk_offset
        // >= ledger_range.start()` and `<= start + max - 1`, so the exclusive
        // partition-relative bound fits u32 and is ≤ `max_partition_offset`.
        let max_plan = PartitionChunkOffset::from(
            u32::try_from(*clamped_end_offset - *ledger_range.start() + 1)
                .unwrap_or(*max_partition_offset),
        );

        IndexRepairPlan::NeedsRepair {
            start_height: start_block,
            end_height: end_block,
            max_partition_offset: max_plan,
        }
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
    /// cannot hang if migration stalls. Deferred and failed blocks (missing data /
    /// index write / oneshot drop / timeout) count toward the returned failure
    /// total (suppresses unblock).
    ///
    /// If `cancel` fires (shutdown requested), the loop returns early before the
    /// next block so a long heal does not stall graceful shutdown. Hard-fails only
    /// if the migration mpsc is closed.
    async fn migrate_storage_module_indexes(
        &self,
        blocks_to_migrate: BTreeMap<u64, BlockHash>,
        cancel: Option<&Shutdown>,
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
            if shutdown_requested(cancel) {
                debug!(
                    block.height = block_height,
                    "shutdown requested during index-heal migration; returning early"
                );
                return Ok(failures);
            }
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
    fn get_max_partition_offset(
        &self,
        storage_module: &Arc<StorageModule>,
    ) -> Option<PartitionChunkOffset> {
        let Some(ledger_id) = storage_module
            .partition_assignment()
            .and_then(|a| a.ledger_id)
        else {
            warn!(
                storage_module.id = storage_module.id,
                "storage module not assigned to a data ledger slot; cannot determine max offset"
            );
            return None;
        };

        let current_height = self.block_tree.read().get_latest_canonical_entry().height();
        let migration_height =
            current_height.saturating_sub(self.config.consensus.block_migration_depth as u64);

        let max_ledger_offset = self
            .block_tree
            .get_total_chunks(migration_height, ledger_id);

        let Ok(range) = storage_module.get_storage_module_ledger_offsets() else {
            warn!(
                storage_module.id = storage_module.id,
                "storage module not assigned to a ledger; cannot determine max offset"
            );
            return None;
        };
        // Inclusive bounds after nodit `ie` → stored inclusive end.
        let start: u64 = *range.start();
        let end_incl: u64 = *range.end();
        let max_excl = max_ledger_offset.map(|m| *m);

        Some(PartitionChunkOffset::from(exclusive_partition_end(
            start, end_incl, max_excl,
        )))
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
    /// Plan found no repairable gap. `reverify_max` is the dense-prefix bound to
    /// re-scan before unblocking (0 if there is no data), so a clear racing the
    /// plan→send window is caught rather than falsely unblocked.
    Complete { reverify_max: PartitionChunkOffset },
    /// Inclusive block heights that should be re-indexed, plus the plan-time
    /// exclusive partition-offset bound. Re-verify uses this snapshot rather than
    /// re-reading the (moving) frontier, so a chain advancing mid-heal cannot
    /// manufacture a false post-heal gap past what this pass planned to repair.
    NeedsRepair {
        start_height: u64,
        end_height: u64,
        max_partition_offset: PartitionChunkOffset,
    },
    /// Gap or bounds uncertainty; heal must not claim success / unblock.
    SoftSkipped,
}

/// Whether the service shutdown signal has fired.
///
/// Polls a fresh clone of the shared `Shutdown` so the caller's handle (the run
/// loop's `&mut self.shutdown`) is never consumed. `now_or_never` polls once
/// with a no-op waker: `Some(())` once the signal fired, `None` while pending.
fn shutdown_requested(cancel: Option<&Shutdown>) -> bool {
    cancel.is_some_and(|s| s.clone().now_or_never().is_some())
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
        //
        // The heal runs before the select! loop, so its per-block migration can be
        // long. Pass a cheap clone of the run loop's `Shutdown` so the heal can bail
        // between blocks if shutdown is requested mid-heal (cloning shares the signal
        // without consuming the `&mut self.shutdown` the loop polls below). Migrate is
        // also chunked + per-response-timeout so a first-gap→frontier repair cannot
        // block startup indefinitely (see migrate_storage_module_indexes).
        let cancel = self.shutdown.clone();
        if let Err(e) = self.inner.heal_ledger_data_indexes(Some(&cancel)).await {
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
                            self.inner.handle_message(msg, Some(&cancel)).instrument(span).await?;
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
            self.inner.handle_message(msg, Some(&cancel)).instrument(span).await?
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

#[cfg(test)]
mod shutdown_requested_tests {
    use super::shutdown_requested;
    use reth::tasks::shutdown::signal;

    #[test]
    fn none_and_pending_are_not_requested() {
        let (_signal, shutdown) = signal();
        // No handle to observe → never cancels (epoch call-site behavior).
        assert!(!shutdown_requested(None));
        // Signal not yet fired → still running.
        assert!(!shutdown_requested(Some(&shutdown)));
    }

    #[test]
    fn fired_signal_is_requested() {
        let (signal, shutdown) = signal();
        assert!(!shutdown_requested(Some(&shutdown)));
        signal.fire();
        // The per-block loop check now short-circuits the heal.
        assert!(shutdown_requested(Some(&shutdown)));
    }

    #[test]
    fn dropped_signal_is_requested() {
        // TokioServiceHandle shutdown also fires by dropping the Signal.
        let (signal, shutdown) = signal();
        drop(signal);
        assert!(shutdown_requested(Some(&shutdown)));
    }
}

/// End-to-end coverage of the per-SM heal -> unblock contract:
///
///   `heal_ledger_data_indexes` must emit `SyncPartitions { unblock_sm_ids }`
///   naming ONLY the SM whose path-hash index is confirmed gap-free, and the
///   data-sync side must then unblock ONLY that SM's `MissingDataRootIndex`
///   offsets while a still-gapped SM stays blocked.
///
/// Two real `StorageModule`s share one registry:
///   - `HEALTHY_ID`: dense path-hash index over its range -> `plan_index_repair`
///     classifies `Complete` (no migration) -> added to `clean_sm_ids`.
///   - `GAPPED_ID`: a real path-hash hole -> `plan_index_repair` classifies
///     `NeedsRepair` -> a minimal chunk-migration responder answers `Ok(())`
///     WITHOUT closing the gap (orphan-skip / TOCTOU shape) -> the post-migration
///     re-verify still finds the hole -> unblock withheld.
///
/// Real code paths exercised: `heal_ledger_data_indexes`, `plan_index_repair`
/// (both the density fast path and the block-index/block-tree-backed repair
/// plan), `migrate_storage_module_indexes` (message send + oneshot await), the
/// post-migration path-hash re-verify, the `SyncPartitions` construction/send,
/// and the real `DataSyncServiceInner` unblock filter over both orchestrators.
/// Stubbed: only the chunk-migration responder (returns `Ok` without indexing,
/// exactly the "migrate returned but gap persists" case the contract guards).
#[cfg(test)]
mod heal_unblock_contract_tests {
    use super::*;
    use crate::DataSyncServiceInner;
    use crate::chunk_migration_service::ChunkMigrationServiceMessage;
    use crate::data_sync_service::chunk_fetcher::{
        ChunkFetcher, ChunkFetcherFactory, MockChunkFetcher,
    };
    use crate::data_sync_service::chunk_orchestrator::{
        ChunkBlockReason, ChunkRequest, ChunkRequestState,
    };
    use crate::test_helpers::build_test_service_senders;
    use irys_config::StorageSubmodulesConfig;
    use irys_database::db::IrysDatabaseExt as _;
    use irys_database::submodule::set_path_hashes_by_offset;
    use irys_database::submodule::tables::ChunkPathHashes;
    use irys_database::{IrysDatabaseArgs as _, open_or_create_db, tables::IrysTables};
    use irys_domain::{BlockIndex, BlockTree, PeerList};
    use irys_testing_utils::IrysBlockHeaderTestExt as _;
    use irys_testing_utils::utils::TempDirBuilder;
    use irys_types::{
        BlockIndexItem, ConsensusConfig, DatabaseProvider, IrysBlockHeader, LedgerIndexItem,
        NodeConfig, partition::PartitionAssignment, partition_chunk_offset_ie,
    };
    use reth_db::mdbx::DatabaseArguments;

    const HEALTHY_ID: usize = 0;
    const GAPPED_ID: usize = 1;
    /// Chunks in the partition / SM ledger span and the ledger frontier: keeping
    /// them equal means the whole SM range is in-frontier and eligible to repair.
    const NUM_CHUNKS: u32 = 4;
    /// Partition offset punched out of the gapped SM's otherwise-dense index.
    const GAP_OFFSET: u32 = 2;

    fn test_config(base_directory: std::path::PathBuf) -> Config {
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: NUM_CHUNKS as u64,
                num_chunks_in_recall_range: 2,
                num_partitions_per_slot: 1,
                entropy_packing_iterations: 1,
                // Only genesis is in the chain, so migration_height clamps to 0
                // regardless of depth; genesis is the migrated frontier block.
                block_migration_depth: 1,
                chain_id: 1,
                ..ConsensusConfig::testing()
            }),
            base_directory,
            ..NodeConfig::testing()
        };
        Config::new_with_random_peer_id(node_config)
    }

    fn ledger_sm(config: &Config, id: usize, dir: &str) -> Arc<StorageModule> {
        let pa = PartitionAssignment {
            ledger_id: Some(DataLedger::Publish.into()),
            slot_index: Some(0),
            miner_address: irys_types::IrysAddress::from([1_u8; 20]),
            partition_hash: irys_types::H256::random(),
        };
        let info = StorageModuleInfo {
            id,
            partition_assignment: Some(pa),
            submodules: vec![(partition_chunk_offset_ie!(0, NUM_CHUNKS), dir.into())],
        };
        Arc::new(StorageModule::new(&info, config).expect("storage module"))
    }

    /// Write a path-hash index entry for every partition offset in `[0, end)`,
    /// making the module's path-hash index dense over that range.
    fn seed_dense_path_hashes(sm: &StorageModule, end: u32) {
        let path_hashes = ChunkPathHashes {
            data_path_hash: Some(irys_types::H256::random()),
            tx_path_hash: Some(irys_types::H256::random()),
        };
        let (_interval, submodule) = sm
            .get_submodule_for_offset(PartitionChunkOffset::from(0))
            .expect("submodule for offset 0");
        submodule
            .db
            .update_eyre(|tx| {
                for offset in 0..end {
                    set_path_hashes_by_offset(
                        tx,
                        PartitionChunkOffset::from(offset),
                        path_hashes.clone(),
                    )?;
                }
                Ok(())
            })
            .expect("seed dense path hashes");
    }

    /// A block-tree genesis and an MDBX-backed block index that both report
    /// `Publish.total_chunks == NUM_CHUNKS`. `plan_index_repair` reads the
    /// frontier from the block tree (`get_total_chunks`) and resolves repair
    /// block heights from the block index (`get_block_bounds`); both must agree.
    fn genesis_block_tree_and_index(
        config: &Config,
    ) -> (BlockTreeReadGuard, BlockIndexReadGuard, DatabaseProvider) {
        let mut genesis = IrysBlockHeader::new_mock_header();
        genesis.height = 0;
        genesis.previous_block_hash = irys_types::H256::zero();
        genesis.data_ledgers[DataLedger::Publish].total_chunks = NUM_CHUNKS as u64;
        genesis.test_sign();

        let block_tree = BlockTree::new(&genesis, config.consensus.clone());
        let block_tree_guard = BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));

        let tmp = TempDirBuilder::new().build();
        let env = open_or_create_db(
            tmp.path(),
            IrysTables::ALL,
            DatabaseArguments::irys_testing().expect("db args"),
        )
        .expect("open block index db");
        let db = DatabaseProvider(Arc::new(env));
        let block_index = BlockIndex::new_for_testing(db.clone());
        block_index
            .push_item(
                &BlockIndexItem {
                    block_hash: genesis.block_hash,
                    num_ledgers: 1,
                    ledgers: vec![LedgerIndexItem {
                        total_chunks: NUM_CHUNKS as u64,
                        tx_root: irys_types::H256::zero(),
                        ledger: DataLedger::Publish,
                    }],
                },
                0,
            )
            .expect("push genesis block index item");
        // Keep the tempdir alive for the DB's lifetime by leaking it into the
        // returned provider's scope — the returned `db` keeps the env open, and
        // the process-scoped test tempdir is cleaned by the harness.
        std::mem::forget(tmp);

        (block_tree_guard, BlockIndexReadGuard::new(block_index), db)
    }

    fn mock_fetcher_factory() -> ChunkFetcherFactory {
        Box::new(|ledger_id| {
            Arc::new(MockChunkFetcher::new(ledger_id as usize)) as Arc<dyn ChunkFetcher>
        })
    }

    #[test_log::test(tokio::test)]
    async fn heal_unblocks_only_gap_free_storage_module() {
        let tmp = TempDirBuilder::new().with_tracing().build();
        let config = test_config(tmp.path().to_path_buf());

        // Healthy SM: dense path-hash index over its whole range.
        let healthy = ledger_sm(&config, HEALTHY_ID, "chunks_healthy");
        seed_dense_path_hashes(&healthy, NUM_CHUNKS);
        assert_eq!(
            healthy
                .first_missing_path_hash_offset(
                    PartitionChunkOffset::from(0),
                    PartitionChunkOffset::from(NUM_CHUNKS)
                )
                .expect("scan healthy"),
            None,
            "precondition: healthy SM index must be gap-free"
        );

        // Gapped SM: dense, then punch a hole so a real gap exists mid-range.
        let gapped = ledger_sm(&config, GAPPED_ID, "chunks_gapped");
        seed_dense_path_hashes(&gapped, NUM_CHUNKS);
        gapped
            .clear_offset_index_in_range(
                PartitionChunkOffset::from(GAP_OFFSET),
                PartitionChunkOffset::from(GAP_OFFSET),
            )
            .expect("punch gap");
        assert_eq!(
            gapped
                .first_missing_path_hash_offset(
                    PartitionChunkOffset::from(0),
                    PartitionChunkOffset::from(NUM_CHUNKS)
                )
                .expect("scan gapped"),
            Some(PartitionChunkOffset::from(GAP_OFFSET)),
            "precondition: gapped SM must have a hole at GAP_OFFSET"
        );

        let storage_modules: Arc<RwLock<Vec<Arc<StorageModule>>>> =
            Arc::new(RwLock::new(vec![healthy, gapped]));

        let (block_tree_guard, block_index_guard, _db) = genesis_block_tree_and_index(&config);

        let (service_senders, mut service_receivers) = build_test_service_senders();

        // Minimal chunk-migration responder: answer every UpdateStorageModuleIndexes
        // with Ok(()) WITHOUT writing any index. This is the adversarial case the
        // per-SM re-verify guards — migration "succeeds" but the gap persists.
        // A shared counter records how many migration requests the heal issued,
        // read after the heal without having to join (senders outlive the heal).
        let mut chunk_migration_rx = std::mem::replace(
            &mut service_receivers.chunk_migration,
            tokio::sync::mpsc::unbounded_channel().1,
        );
        let migrate_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let migrate_calls_task = migrate_calls.clone();
        let responder = tokio::spawn(async move {
            while let Some(traced) = chunk_migration_rx.recv().await {
                let (msg, _span) = traced.into_parts();
                if let ChunkMigrationServiceMessage::UpdateStorageModuleIndexes {
                    receiver, ..
                } = msg
                {
                    // Deliberately do NOT index — leave the gap in place.
                    let _ = receiver.send(Ok(()));
                    migrate_calls_task.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            }
        });

        let submodules_config =
            StorageSubmodulesConfig::load_for_test(config.node_config.base_directory.clone(), 2)
                .expect("submodules config");

        let inner = StorageModuleServiceInner {
            storage_modules: storage_modules.clone(),
            block_index: block_index_guard.clone(),
            block_tree: block_tree_guard.clone(),
            submodules_config,
            service_senders: service_senders.clone(),
            config: config.clone(),
        };

        // Drive the real heal pass (plan -> migrate -> re-verify -> SyncPartitions).
        inner
            .heal_ledger_data_indexes(None)
            .await
            .expect("heal_ledger_data_indexes");

        // Capture the SyncPartitions the heal emitted.
        let traced = service_receivers
            .data_sync
            .try_recv()
            .expect("heal must emit a SyncPartitions message");
        let (msg, _span) = traced.into_parts();
        let unblock_sm_ids = match msg {
            DataSyncServiceMessage::SyncPartitions { unblock_sm_ids } => unblock_sm_ids,
            _ => panic!("expected a SyncPartitions message from the heal pass"),
        };

        assert!(
            unblock_sm_ids.contains(&HEALTHY_ID),
            "gap-free SM must be in unblock_sm_ids, got {unblock_sm_ids:?}"
        );
        assert!(
            !unblock_sm_ids.contains(&GAPPED_ID),
            "still-gapped SM must NOT be in unblock_sm_ids, got {unblock_sm_ids:?}"
        );

        // ---- data-sync half: the captured message must unblock only HEALTHY ----
        let peer_list = PeerList::from_peers(
            vec![],
            service_senders.peer_network.clone(),
            &config,
            tokio::sync::broadcast::channel::<irys_domain::PeerEvent>(100).0,
        )
        .expect("peer list");

        let mut data_sync = DataSyncServiceInner::new(
            block_tree_guard,
            storage_modules,
            peer_list,
            mock_fetcher_factory(),
            service_senders,
            config,
            tokio::runtime::Handle::current(),
        );

        // One orchestrator per ledger-assigned SM.
        assert!(data_sync.chunk_orchestrators.contains_key(&HEALTHY_ID));
        assert!(data_sync.chunk_orchestrators.contains_key(&GAPPED_ID));

        // Seed each orchestrator with a MissingDataRootIndex-blocked offset.
        let blocked_offset = PartitionChunkOffset::from(0_u32);
        for id in [HEALTHY_ID, GAPPED_ID] {
            let orch = data_sync.chunk_orchestrators.get_mut(&id).unwrap();
            orch.chunk_requests.insert(
                blocked_offset,
                ChunkRequest {
                    ledger_id: DataLedger::Publish as usize,
                    slot_index: 0,
                    chunk_offset: blocked_offset,
                    excluded: None,
                    request_state: ChunkRequestState::Blocked(
                        ChunkBlockReason::MissingDataRootIndex,
                    ),
                },
            );
        }

        // Feed the exact message the heal produced.
        data_sync
            .handle_message(DataSyncServiceMessage::SyncPartitions { unblock_sm_ids })
            .expect("handle SyncPartitions");

        assert_eq!(
            data_sync.chunk_orchestrators[&HEALTHY_ID].chunk_requests[&blocked_offset]
                .request_state,
            ChunkRequestState::Pending,
            "healthy SM's blocked offset must be re-queued to Pending"
        );
        assert_eq!(
            data_sync.chunk_orchestrators[&GAPPED_ID].chunk_requests[&blocked_offset].request_state,
            ChunkRequestState::Blocked(ChunkBlockReason::MissingDataRootIndex),
            "gapped SM's offset must stay Blocked (withheld by re-verify)"
        );

        // The gapped SM's repair plan must have driven at least one migration
        // request (proving it reached NeedsRepair, not a trivial Complete/SoftSkip).
        assert!(
            migrate_calls.load(std::sync::atomic::Ordering::SeqCst) >= 1,
            "gapped SM must have triggered a migration request (reached NeedsRepair)"
        );
        responder.abort();
    }
}
