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
mod index_heal;

use crate::{packing_service::PackingRequest, services::ServiceSenders};
use eyre::OptionExt as _;
use index_heal::{HealOutcome, INDEX_HEAL_RETRY_INTERVAL, IndexHealCtx, heal_ledger_data_indexes};
use irys_config::StorageSubmodulesConfig;
use irys_domain::{
    BlockIndexReadGuard, BlockTreeReadGuard, PACKING_PARAMS_FILE_NAME, PackingParams,
    StorageModule, StorageModuleInfo,
};
use irys_types::{Config, PartitionChunkRange, TokioServiceHandle, Traced};
use reth::tasks::shutdown::Shutdown;
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{Instrument as _, debug, error, warn};

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

pub struct StorageModuleServiceInner {
    storage_modules: Arc<RwLock<Vec<Arc<StorageModule>>>>,
    block_index: BlockIndexReadGuard,
    block_tree: BlockTreeReadGuard,
    submodules_config: StorageSubmodulesConfig,
    service_senders: ServiceSenders,
    config: Config,
    /// Same signal as the run loop — migrate aborts mid-heal when shutdown fires.
    /// Not `Debug` (reth `Shutdown`); service type stays printable via outer handle.
    shutdown: Shutdown,
    /// Last heal pass left unrepaired work — run periodic heal until clear.
    index_heal_needs_retry: bool,
}

impl std::fmt::Debug for StorageModuleServiceInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorageModuleServiceInner")
            .field("index_heal_needs_retry", &self.index_heal_needs_retry)
            .finish_non_exhaustive()
    }
}

impl StorageModuleServiceInner {
    /// Create a new StorageModuleServiceInner instance
    pub fn new(
        storage_modules: Arc<RwLock<Vec<Arc<StorageModule>>>>,
        block_index: BlockIndexReadGuard,
        block_tree: BlockTreeReadGuard,
        service_senders: ServiceSenders,
        config: Config,
        shutdown: Shutdown,
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
            shutdown,
            index_heal_needs_retry: true, // force heal until first pass reports clean
        }
    }

    fn index_heal_ctx(&self) -> IndexHealCtx<'_> {
        IndexHealCtx {
            storage_modules: &self.storage_modules,
            block_index: &self.block_index,
            block_tree: &self.block_tree,
            service_senders: &self.service_senders,
            config: &self.config,
        }
    }

    async fn run_index_heal(&mut self) -> eyre::Result<HealOutcome> {
        let outcome = heal_ledger_data_indexes(&self.index_heal_ctx(), &self.shutdown).await?;
        self.index_heal_needs_retry = outcome.needs_retry;
        Ok(outcome)
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
        let storage_modules = {
            let guard = self.storage_modules.read().unwrap();
            guard.clone()
        };

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

        // After assignments settle: best-effort gap-scan + index backfill for every
        // local SM on a data ledger. Do not fail membership updates if heal hard-
        // errors (channel closed); periodic retry picks up via needs_retry.
        if let Err(e) = self.run_index_heal().await {
            error!("index heal after partition assignment failed: {e:?}");
            self.index_heal_needs_retry = true;
        }

        Ok(())
    }

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
        // Clone for Inner so heal can bail on shutdown without threading cancel
        // through the message API. Shares the same signal as the run-loop poll.
        let shutdown_for_inner = shutdown_rx.clone();

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
                        shutdown_for_inner,
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

        // Soft-skips per-SM issues; only migration channel closed is fatal.
        // Inner holds a Shutdown clone so migrate bails mid-heal without a cancel param.
        if let Err(e) = self.inner.run_index_heal().await {
            error!("startup data-index heal failed: {e:?}");
            return Err(e);
        }

        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.tick().await; // Skip first immediate tick
        let mut heal_interval = tokio::time::interval(INDEX_HEAL_RETRY_INTERVAL);
        heal_interval.tick().await; // Skip first immediate (startup already healed)

        loop {
            tokio::select! {
                biased;

                _ = &mut self.shutdown => {
                    tracing::info!("Shutdown signal received for storage module service");
                    break;
                }
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
                // Only when the last pass left unrepaired work (multi-pass cap /
                // soft-skips). Healthy nodes stay quiet between epochs.
                _ = heal_interval.tick(), if self.inner.index_heal_needs_retry => {
                    if let Err(e) = self.inner.run_index_heal().await {
                        error!("periodic data-index heal failed: {e:?}");
                    }
                }
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
