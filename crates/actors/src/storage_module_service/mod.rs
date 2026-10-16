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
    thread::JoinHandle,
    time::Duration,
};
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{Instrument as _, debug, error, warn};

// Messages that the StorageModuleService service supports
#[derive(Debug, Clone)]
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
    /// Assignment update whose reset did not finish. The next tick retries it.
    pending_assignment: Option<StorageModuleServiceMessage>,
    /// Spans the packing channel did not accept. `tick` sends them until the
    /// channel accepts. A range stays one interval, so a full partition is not
    /// expanded into one repair offset per chunk.
    pending_ranges: HashMap<usize, Vec<PartitionChunkRange>>,
}

/// Reset did not finish. The assignment is unchanged. The service stays up
/// and retries the same update.
#[derive(Debug)]
struct ResetNotFinished;

impl std::fmt::Display for ResetNotFinished {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("storage module reset did not finish")
    }
}

impl std::error::Error for ResetNotFinished {}

/// Outcome of one non-blocking send to the packing channel.
enum PackSend {
    Accepted,
    /// The channel is full or closed. The caller keeps the range.
    Deferred,
    /// The request is not valid. An unassigned module is not retried.
    Rejected,
}

fn is_reset_not_finished(err: &eyre::Report) -> bool {
    err.downcast_ref::<ResetNotFinished>().is_some()
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
            pending_assignment: None,
            pending_ranges: HashMap::new(),
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

    /// Apply one assignment update. A reset that does not finish is stored and
    /// retried. The service stays up. Any other error still aborts it.
    async fn apply_assignment(&mut self, msg: StorageModuleServiceMessage) -> eyre::Result<()> {
        match self.handle_message(msg.clone()).await {
            Ok(()) => {
                self.pending_assignment = None;
                Ok(())
            }
            Err(err) if is_reset_not_finished(&err) => {
                debug!(
                    "storage module reset did not finish; the assignment update will be retried"
                );
                self.pending_assignment = Some(msg);
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    fn spawn_disk_lanes(&self) -> DiskLaneSet {
        let modules = {
            let guard = self.storage_modules.read().unwrap();
            guard.clone()
        };
        let mut handles = Vec::new();
        for module in modules {
            match module.spawn_disk_lane() {
                Ok(Some(handle)) => handles.push((module, handle)),
                Ok(None) => {}
                Err(error) => {
                    error!(
                        "Couldn't start disk lane for storage_module {}: {error}",
                        module.id
                    );
                }
            }
        }
        DiskLaneSet { handles }
    }

    #[tracing::instrument(level = "trace", skip_all)]
    fn tick(&mut self) {
        let storage_modules = {
            let guard = self.storage_modules.read().unwrap();
            guard.clone()
        };

        for sm in storage_modules.iter() {
            // Repair stays queued while the lane is up. The lane early-continue
            // below must not skip it. Confirm recall suspects first, on this
            // thread: the mining thread must not take `sync_in_progress`.
            sm.confirm_crc_suspects();
            self.enqueue_crc_repairs(sm);
            if sm.disk_lane_running() {
                sm.wake_disk_lane();
                continue;
            }
            // The lane thread is the flush and the sweep. This path covers a
            // module whose thread failed to start.
            if sm.has_pending_writes() {
                // Check the configured per-submodule batch threshold once per
                // service tick, not once per ingested chunk.
                if let Err(e) = sm.sync_pending_chunks() {
                    error!(
                        "Couldn't sync buffered chunks for storage_module {}: {}",
                        sm.id, e
                    );
                }
                // `last_pending_write` is monotonic. Force only an idle tail;
                // active ingress continues to benefit from striped batches.
                if sm.has_pending_writes()
                    && sm.last_pending_write().elapsed() > Duration::from_secs(5)
                    && let Err(e) = sm.force_sync_pending_chunks()
                {
                    error!(
                        "Couldn't flush idle pending chunks for storage_module {}: {}",
                        sm.id, e
                    );
                }
            }
            sm.pump_entropy_reads();
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
                    self.clear_assignment_if_outdated(
                        module,
                        update_height,
                        "missing_from_update",
                    )?;
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
                        )?;
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
                // Reset and install the assignment under one write lock. A
                // failed reset leaves the module unassigned, so the next
                // update takes this branch again.
                let range = self.reset_and_install(existing, Some(assignment), update_height)?;
                // The protocol assigns a new partition to capacity for one epoch.
                self.enqueue_packing(existing, range);

                // Skip any further validations for now
                continue;
            }

            // Path equality for every update entry was checked in the pass above.
            let path = &self.submodules_config.submodule_paths[sm_info.id];

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

                    // A ledger slot is changing. Clear the old index and store
                    // the final assignment under one write lock. That covers
                    // ledger→capacity and a same-epoch ledger→ledger
                    // reassignment. A failed reset leaves the previous
                    // assignment, so the next update still sees the change.
                    // Capacity→ledger does not reset: index repair runs below.
                    if ledger_before.is_some() {
                        let range =
                            self.reset_and_install(existing, Some(info_pa), update_height)?;
                        self.enqueue_packing(existing, range);
                    } else {
                        // Capacity→ledger does not reset and must not repack.
                        // Drop a span the capacity reset could not send.
                        if let Err(err) = existing.assign_partition(info_pa, update_height) {
                            // The packing-params write failed before memory changed.
                            // Retry the same update. Do not stop the service.
                            warn!(
                                storage_module.id = existing.id,
                                "capacity-to-ledger assignment was not stored; the update will be retried: {err:#}"
                            );
                            return Err(ResetNotFinished.into());
                        }
                        self.pending_ranges.remove(&existing.id);
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

    /// Finish a reset that stopped before its marker was dropped.
    /// The assignment is already `None` and is not written again.
    fn finish_reset(&self, module: &Arc<StorageModule>) -> eyre::Result<PartitionChunkRange> {
        self.map_reset(module, module.reset())
    }

    /// Reset and install `next` as one critical section. A failed reset
    /// leaves the previous assignment in place so the update can be retried.
    fn reset_and_install(
        &self,
        module: &Arc<StorageModule>,
        next: Option<irys_types::partition::PartitionAssignment>,
        update_height: u64,
    ) -> eyre::Result<PartitionChunkRange> {
        self.map_reset(module, module.reset_with_assignment(next, update_height))
    }

    fn map_reset(
        &self,
        module: &Arc<StorageModule>,
        result: eyre::Result<nodit::Interval<irys_types::PartitionChunkOffset>>,
    ) -> eyre::Result<PartitionChunkRange> {
        match result {
            Ok(interval) => Ok(PartitionChunkRange(interval)),
            Err(err) => {
                warn!(
                    storage_module.id = module.id,
                    "storage module reset failed: {err}"
                );
                Err(ResetNotFinished.into())
            }
        }
    }

    /// Ask the packing service to fill the range. A full or closed channel
    /// does not fail the assignment update: `tick` sends the span later.
    fn enqueue_packing(&mut self, module: &Arc<StorageModule>, range: PartitionChunkRange) {
        // This span replaces any range the channel has not accepted. The
        // reset already cleared the old bytes.
        self.pending_ranges.remove(&module.id);
        if matches!(self.try_enqueue_packing(module, range), PackSend::Deferred) {
            self.pending_ranges.insert(module.id, vec![range]);
        }
    }

    /// No partition assignment is not a warning: packing cannot run.
    fn try_enqueue_packing(
        &self,
        module: &Arc<StorageModule>,
        range: PartitionChunkRange,
    ) -> PackSend {
        let sender = self.service_senders.packing_sender();
        let Ok(req) = PackingRequest::new(module.clone(), range) else {
            return PackSend::Rejected;
        };
        match sender.try_send(req) {
            Ok(()) => PackSend::Accepted,
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!(
                    target = "irys::packing",
                    storage_module.id = %module.id,
                    storage_module.packing_interval = ?range,
                    "Dropping packing request due to a saturated channel"
                );
                PackSend::Deferred
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_req)) => {
                tracing::error!(
                    target = "irys::packing",
                    storage_module.id = %module.id,
                    storage_module.packing_interval = ?range,
                    "Packing channel closed; failed to enqueue repacking request"
                );
                PackSend::Deferred
            }
        }
    }

    /// Hand crc mismatches to the packing queue. A full channel keeps the
    /// range here, as one interval. An unassigned module drops it.
    fn enqueue_crc_repairs(&mut self, module: &Arc<StorageModule>) {
        if module.partition_assignment().is_none() {
            // The module cannot pack. Leaving the range queued makes every
            // later tick expand it and try again.
            let _dropped = module.take_crc_repair_ranges();
            self.pending_ranges.remove(&module.id);
            return;
        }

        let mut waiting = self.pending_ranges.remove(&module.id).unwrap_or_default();
        waiting.extend(
            module
                .take_crc_repair_ranges()
                .into_iter()
                .map(PartitionChunkRange),
        );
        let mut deferred = Vec::new();
        for range in waiting {
            match self.try_enqueue_packing(module, range) {
                PackSend::Accepted => {}
                PackSend::Deferred => deferred.push(range),
                PackSend::Rejected => {
                    // `new` failed while the module is still assigned: the
                    // range does not fit the partition. Put that range back.
                    // An assignment cleared during the send is not retried.
                    if module.partition_assignment().is_some() {
                        module.requeue_crc_repair_range(range.0);
                    }
                }
            }
        }
        if !deferred.is_empty() {
            self.pending_ranges.insert(module.id, deferred);
        }
    }

    fn clear_assignment_if_outdated(
        &mut self,
        module: &Arc<StorageModule>,
        update_height: u64,
        reason: &'static str,
    ) -> eyre::Result<()> {
        if module.partition_assignment().is_none() {
            // The assignment is already gone. A span kept for a full channel
            // must not be packed later. Finish a reset that stopped before
            // its marker was dropped. Do not write the assignment again.
            self.pending_ranges.remove(&module.id);
            if module.reset_incomplete() {
                self.finish_reset(module)?;
            }
            return Ok(());
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
            return Ok(());
        }

        debug!(
            storage_module.id = module.id,
            storage_module.update_height = update_height,
            storage_module.clear_reason = reason,
            "clearing local partition assignment"
        );
        // Reset and clear the assignment under one write lock. A failure
        // leaves the assignment in place so the next update retries.
        let range = self.reset_and_install(module, None, update_height)?;
        // The module is unassigned. Do not pack a span the channel deferred.
        self.pending_ranges.remove(&module.id);
        debug!(
            packing.interval = ?range,
            storage_module.id = module.id,
            storage_module.clear_reason = reason,
            "storage module reset before unassign"
        );
        Ok(())
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
        let _lanes = self.inner.spawn_disk_lanes();

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
                            // A newer update replaces one that is waiting to retry.
                            self.inner.pending_assignment = None;
                            let span = tracing::trace_span!(parent: &parent_span, "storage_module_handle_message");
                            self.inner.apply_assignment(msg).instrument(span).await?;
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
                    if let Some(pending) = self.inner.pending_assignment.clone() {
                        self.inner.apply_assignment(pending).await?;
                    }
                    self.inner.tick();
                }
            }
        }

        tracing::debug!(custom.amount_of_messages = ?self.msg_rx.len(), "processing last in-bound messages before shutdown");
        if let Some(pending) = self.inner.pending_assignment.clone() {
            self.inner.apply_assignment(pending).await?;
        }
        while let Ok(traced) = self.msg_rx.try_recv() {
            let (msg, parent_span) = traced.into_parts();
            self.inner.pending_assignment = None;
            let span = tracing::trace_span!(parent: &parent_span, "storage_module_handle_message");
            self.inner.apply_assignment(msg).instrument(span).await?
        }

        tracing::info!("shutting down StorageModule Service gracefully");
        Ok(())
    }
}

struct DiskLaneSet {
    handles: Vec<(Arc<StorageModule>, JoinHandle<()>)>,
}

impl Drop for DiskLaneSet {
    fn drop(&mut self) {
        for (module, _) in &self.handles {
            module.stop_disk_lane();
        }
        for (_, handle) in self.handles.drain(..) {
            if let Err(payload) = handle.join() {
                error!("disk lane thread panicked: {payload:?}");
            }
        }
    }
}

#[cfg(test)]
mod assignment_reset_tests {
    use super::*;
    use crate::services::ServiceSenders;
    use irys_config::StorageSubmodulesConfig;
    use irys_database::{IrysDatabaseArgs as _, open_or_create_db, tables::IrysTables};
    use irys_domain::{
        BlockIndex, BlockTree, BlockTreeReadGuard, StorageModule, StorageModuleInfo,
        block_index_guard::BlockIndexReadGuard,
    };
    use irys_testing_utils::{new_mock_signed_header, utils::TempDirBuilder};
    use irys_types::{
        Config, ConsensusConfig, ConsensusOptions, DataLedger, DatabaseProvider, H256, NodeConfig,
        NodeMode, PartitionChunkOffset, ii, partition::PartitionAssignment,
        partition_chunk_offset_ie,
    };
    use reth_db::mdbx::DatabaseArguments;
    use std::sync::{Arc, RwLock};
    use tokio::sync::mpsc::error::TrySendError;

    struct Harness {
        _tmp: irys_testing_utils::tempfile::TempDir,
        _shutdown_tx: reth::tasks::shutdown::Signal,
        receivers: crate::services::ServiceReceivers,
        inner: StorageModuleServiceInner,
        module: Arc<StorageModule>,
        path: std::path::PathBuf,
    }

    fn harness(ledger_id: Option<u32>) -> Harness {
        open_harness(ledger_id, |_| {})
    }

    fn open_harness(ledger_id: Option<u32>, configure: impl FnOnce(&mut NodeConfig)) -> Harness {
        let tmp = TempDirBuilder::new().build();
        let mut node_config = NodeConfig::testing();
        node_config.node_mode = NodeMode::Miner;
        node_config.base_directory = tmp.path().to_path_buf();
        node_config.consensus = ConsensusOptions::Custom(ConsensusConfig {
            chunk_size: 32,
            num_chunks_in_partition: 10,
            ..ConsensusConfig::testing()
        });
        configure(&mut node_config);
        let submodules = StorageSubmodulesConfig::load_for_test(tmp.path().to_path_buf(), 1)
            .expect("submodules");
        let path = submodules.submodule_paths[0].clone();
        let config = Config::new_with_random_peer_id(node_config);

        let partition_hash = H256::repeat_byte(0x21);
        let info = StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment {
                partition_hash,
                miner_address: config.node_config.miner_address(),
                ledger_id,
                slot_index: ledger_id.map(|_| 0),
            }),
            submodules: vec![(partition_chunk_offset_ie!(0, 10), path.clone())],
        };
        let module = Arc::new(StorageModule::new(&info, &config).expect("storage module"));

        let db_env = open_or_create_db(
            tmp.path().join("chain-db"),
            IrysTables::ALL,
            DatabaseArguments::irys_testing().expect("db args"),
        )
        .expect("chain db");
        let block_index = BlockIndex::new_for_testing(DatabaseProvider(Arc::new(db_env)));
        let block_index_guard = BlockIndexReadGuard::new(block_index);
        let block_tree = BlockTree::new(&new_mock_signed_header(), config.consensus.clone());
        let block_tree_guard = BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));
        let (service_senders, receivers) = ServiceSenders::new();
        let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();
        let inner = StorageModuleServiceInner::new(
            Arc::new(RwLock::new(vec![Arc::clone(&module)])),
            block_index_guard,
            block_tree_guard,
            service_senders,
            config,
            shutdown_rx,
        );
        Harness {
            _tmp: tmp,
            _shutdown_tx: shutdown_tx,
            receivers,
            inner,
            module,
            path,
        }
    }

    fn capacity_update(
        path: &std::path::Path,
        hash: H256,
        miner: irys_types::IrysAddress,
    ) -> Arc<Vec<StorageModuleInfo>> {
        Arc::new(vec![StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment {
                partition_hash: hash,
                miner_address: miner,
                ledger_id: None,
                slot_index: None,
            }),
            submodules: vec![(partition_chunk_offset_ie!(0, 10), path.to_path_buf())],
        }])
    }

    #[tokio::test]
    async fn expiry_reset_failure_keeps_the_ledger_and_retries() {
        let mut harness = harness(Some(DataLedger::Publish.into()));
        let hash = harness.module.partition_hash().expect("assigned");
        let miner = harness
            .module
            .partition_assignment()
            .expect("assigned")
            .miner_address;
        let update = capacity_update(&harness.path, hash, miner);

        harness.module.fail_next_reset_for_test();
        harness
            .inner
            .apply_assignment(StorageModuleServiceMessage::PartitionAssignmentsUpdated {
                storage_module_infos: Arc::clone(&update),
                update_height: 4,
            })
            .await
            .expect("reset failure is retried, not fatal");
        assert!(harness.inner.pending_assignment.is_some());
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            Some(DataLedger::Publish.into())
        );
        assert!(harness.module.reset_incomplete());
        assert!(harness.receivers.packing.try_recv().is_err());

        harness
            .inner
            .apply_assignment(StorageModuleServiceMessage::PartitionAssignmentsUpdated {
                storage_module_infos: update,
                update_height: 4,
            })
            .await
            .expect("retry finishes the reset");
        assert!(harness.inner.pending_assignment.is_none());
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            None
        );
        assert!(!harness.module.reset_incomplete());
        assert!(harness.receivers.packing.try_recv().is_ok());
    }

    /// A hash that expired and was backfilled onto another ledger slot in the
    /// same epoch arrives here as a ledger-to-ledger change. The old index
    /// belongs to the previous slot. The reset stores the new slot.
    #[tokio::test]
    async fn ledger_reassignment_clears_the_old_index() {
        use irys_domain::storage_module::ChunkType;

        let mut harness = harness(Some(DataLedger::Submit.into()));
        let hash = harness.module.partition_hash().expect("assigned");
        let miner = harness
            .module
            .partition_assignment()
            .expect("assigned")
            .miner_address;
        let offset = PartitionChunkOffset::from(0);
        assert!(
            harness
                .module
                .write_chunk(offset, vec![0x11_u8; 32], ChunkType::Data)
        );
        harness
            .module
            .force_sync_pending_chunks()
            .expect("sync data");
        assert_eq!(
            harness.module.get_chunk_type(&offset),
            Some(ChunkType::Data)
        );

        let update = Arc::new(vec![StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment {
                partition_hash: hash,
                miner_address: miner,
                ledger_id: Some(DataLedger::Publish.into()),
                slot_index: Some(1),
            }),
            submodules: vec![(partition_chunk_offset_ie!(0, 10), harness.path.clone())],
        }]);
        harness
            .inner
            .apply_assignment(StorageModuleServiceMessage::PartitionAssignmentsUpdated {
                storage_module_infos: update,
                update_height: 8,
            })
            .await
            .expect("reassignment");

        let assignment = harness.module.partition_assignment().expect("assigned");
        assert_eq!(assignment.partition_hash, hash);
        assert_eq!(assignment.ledger_id, Some(DataLedger::Publish.into()));
        assert_eq!(assignment.slot_index, Some(1));
        assert_eq!(
            harness.module.get_chunk_type(&offset),
            Some(ChunkType::Uninitialized),
            "the previous slot's bytes are not the new slot's index"
        );
        assert!(harness.receivers.packing.try_recv().is_ok());
    }

    #[tokio::test]
    async fn unassign_reset_failure_keeps_the_assignment() {
        let mut harness = harness(Some(DataLedger::Publish.into()));
        let path = harness.path.clone();
        let update = Arc::new(vec![StorageModuleInfo {
            id: 0,
            partition_assignment: None,
            submodules: vec![(partition_chunk_offset_ie!(0, 10), path)],
        }]);

        harness.module.fail_next_reset_for_test();
        let err = harness
            .inner
            .handle_partition_assignments_update(Arc::clone(&update), 4)
            .await
            .expect_err("reset failure");
        assert!(super::is_reset_not_finished(&err));
        assert!(harness.module.partition_assignment().is_some());
        assert!(harness.module.reset_incomplete());

        harness
            .inner
            .handle_partition_assignments_update(update, 4)
            .await
            .expect("retry clears the assignment");
        assert!(harness.module.partition_assignment().is_none());
        assert!(!harness.module.reset_incomplete());
        assert!(harness.receivers.packing.try_recv().is_err());
    }

    /// A queued checksum repair is handed to packing even when the disk lane
    /// is running. The lane early-continue in `tick` must not skip that send.
    #[tokio::test]
    async fn crc_repair_is_enqueued_while_the_disk_lane_runs() {
        use irys_domain::storage_module::ChunkType;
        use irys_types::{StorageSyncConfig, SubmoduleIndexEngine};
        use std::os::unix::fs::FileExt as _;

        let mut harness = open_harness(Some(DataLedger::Publish.into()), |node_config| {
            node_config.storage = StorageSyncConfig {
                num_writes_before_sync: 1,
                intervals_persist_secs: 0,
                min_durable_interval_ms: 0,
                ..StorageSyncConfig::default()
            };
            node_config.database.submodule_index = SubmoduleIndexEngine::Rocksdb;
        });
        assert!(harness.module.write_chunk(
            PartitionChunkOffset::from(0),
            vec![0x31_u8; 32],
            ChunkType::Entropy,
        ));
        harness
            .module
            .force_sync_pending_chunks()
            .expect("entropy is durable");
        let chunks = harness.path.join("chunks.dat");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&chunks)
            .expect("chunks.dat");
        let mut byte = [0_u8; 1];
        file.read_exact_at(&mut byte, 0).expect("read byte");
        byte[0] ^= 0xff;
        file.write_all_at(&byte, 0).expect("flip byte");
        drop(file);
        let served = harness
            .module
            .read_chunks(partition_chunk_offset_ie!(0, 1))
            .expect("read");
        assert!(
            served.is_empty(),
            "the flipped chunk is omitted: {served:?}"
        );

        let handle = harness
            .module
            .spawn_disk_lane()
            .expect("spawn")
            .expect("lane started");
        struct Stop {
            module: Arc<StorageModule>,
            handle: Option<std::thread::JoinHandle<()>>,
        }
        impl Drop for Stop {
            fn drop(&mut self) {
                self.module.stop_disk_lane();
                if let Some(handle) = self.handle.take() {
                    let _ = handle.join();
                }
            }
        }
        let _stop = Stop {
            module: Arc::clone(&harness.module),
            handle: Some(handle),
        };
        assert!(harness.module.disk_lane_running());
        harness.inner.tick();
        let request = harness
            .receivers
            .packing
            .try_recv()
            .expect("crc repair is enqueued while the disk lane is running");
        assert_eq!(*request.chunk_range().0.start(), 0);
        assert!(
            harness.module.take_crc_repair_ranges().is_empty(),
            "the tick drained the repair"
        );
    }

    fn publish_update(
        path: &std::path::Path,
        hash: H256,
        miner: irys_types::IrysAddress,
    ) -> Arc<Vec<StorageModuleInfo>> {
        Arc::new(vec![StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment {
                partition_hash: hash,
                miner_address: miner,
                ledger_id: Some(DataLedger::Publish.into()),
                slot_index: Some(0),
            }),
            submodules: vec![(partition_chunk_offset_ie!(0, 10), path.to_path_buf())],
        }])
    }

    fn set_dir_mode(path: &std::path::Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt as _;
        let mut perms = std::fs::metadata(path).expect("metadata").permissions();
        perms.set_mode(mode);
        std::fs::set_permissions(path, perms).expect("set permissions");
    }

    /// Restores directory writes if the test exits while the dir is read-only.
    struct WritableDir(std::path::PathBuf);

    impl Drop for WritableDir {
        fn drop(&mut self) {
            set_dir_mode(&self.0, 0o755);
        }
    }

    /// A packing-params write error on capacity→ledger is retried. The
    /// in-memory assignment stays capacity. A later successful write stores
    /// the ledger. A path mismatch is still fatal.
    #[tokio::test]
    async fn capacity_to_ledger_write_error_is_retried() {
        let mut harness = harness(None);
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            None
        );
        let hash = harness.module.partition_hash().expect("assigned");
        let miner = harness
            .module
            .partition_assignment()
            .expect("assigned")
            .miner_address;
        let update = publish_update(&harness.path, hash, miner);
        let _restore = WritableDir(harness.path.clone());
        set_dir_mode(&harness.path, 0o555);

        harness
            .inner
            .apply_assignment(StorageModuleServiceMessage::PartitionAssignmentsUpdated {
                storage_module_infos: Arc::clone(&update),
                update_height: 6,
            })
            .await
            .expect("a packing-params write error is retried, not fatal");
        assert!(
            harness.inner.pending_assignment.is_some(),
            "the update is stored for the retry tick"
        );
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            None,
            "a failed write leaves the in-memory assignment unchanged"
        );
        assert!(!harness.module.reset_incomplete());
        assert!(harness.receivers.packing.try_recv().is_err());

        set_dir_mode(&harness.path, 0o755);
        harness
            .inner
            .apply_assignment(StorageModuleServiceMessage::PartitionAssignmentsUpdated {
                storage_module_infos: update,
                update_height: 6,
            })
            .await
            .expect("retry stores the ledger");
        assert!(harness.inner.pending_assignment.is_none());
        let assignment = harness.module.partition_assignment().unwrap();
        assert_eq!(assignment.ledger_id, Some(DataLedger::Publish.into()));
        assert_eq!(assignment.slot_index, Some(0));
        assert_eq!(assignment.partition_hash, hash);
    }

    /// Capacity→ledger does not reset. Packed bytes stay, and packing is not
    /// asked to rewrite the partition.
    #[tokio::test]
    async fn capacity_to_ledger_keeps_packed_bytes() {
        use irys_domain::storage_module::ChunkType;

        let mut harness = harness(None);
        let offset = PartitionChunkOffset::from(0);
        assert!(
            harness
                .module
                .write_chunk(offset, vec![0x5A_u8; 32], ChunkType::Entropy,)
        );
        harness
            .module
            .force_sync_pending_chunks()
            .expect("entropy is durable");
        assert_eq!(
            harness.module.get_chunk_type(&offset),
            Some(ChunkType::Entropy)
        );

        let hash = harness.module.partition_hash().expect("assigned");
        let miner = harness
            .module
            .partition_assignment()
            .expect("assigned")
            .miner_address;
        harness
            .inner
            .apply_assignment(StorageModuleServiceMessage::PartitionAssignmentsUpdated {
                storage_module_infos: publish_update(&harness.path, hash, miner),
                update_height: 7,
            })
            .await
            .expect("capacity to ledger");
        assert!(harness.inner.pending_assignment.is_none());
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            Some(DataLedger::Publish.into())
        );
        assert_eq!(
            harness.module.get_chunk_type(&offset),
            Some(ChunkType::Entropy),
            "capacity to ledger stores the assignment without resetting the index"
        );
        assert!(
            harness.receivers.packing.try_recv().is_err(),
            "capacity to ledger does not enqueue a reset pack"
        );
    }

    #[tokio::test]
    async fn assignment_path_mismatch_is_not_retried() {
        let mut harness = harness(None);
        let hash = harness.module.partition_hash().expect("assigned");
        let miner = harness
            .module
            .partition_assignment()
            .expect("assigned")
            .miner_address;
        let update = Arc::new(vec![StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment {
                partition_hash: hash,
                miner_address: miner,
                ledger_id: Some(DataLedger::Publish.into()),
                slot_index: Some(0),
            }),
            submodules: vec![(
                partition_chunk_offset_ie!(0, 10),
                harness.path.join("not-the-module"),
            )],
        }]);
        let err = harness
            .inner
            .apply_assignment(StorageModuleServiceMessage::PartitionAssignmentsUpdated {
                storage_module_infos: update,
                update_height: 8,
            })
            .await
            .expect_err("a path mismatch is not an I/O retry");
        assert!(
            !super::is_reset_not_finished(&err),
            "path mismatch must not take the reset retry: {err:#}"
        );
        assert!(harness.inner.pending_assignment.is_none());
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            None
        );
    }

    fn assigned_hash_and_miner(module: &StorageModule) -> (H256, irys_types::IrysAddress) {
        let assignment = module.partition_assignment().expect("assigned");
        (assignment.partition_hash, assignment.miner_address)
    }

    async fn apply_update(harness: &mut Harness, infos: Arc<Vec<StorageModuleInfo>>, height: u64) {
        harness
            .inner
            .apply_assignment(StorageModuleServiceMessage::PartitionAssignmentsUpdated {
                storage_module_infos: infos,
                update_height: height,
            })
            .await
            .expect("assignment update");
    }

    fn fill_packing_channel(harness: &Harness) {
        let sender = harness.inner.service_senders.packing_sender();
        let range = PartitionChunkRange(irys_types::partition_chunk_offset_ii!(0, 0));
        loop {
            let request =
                PackingRequest::new(Arc::clone(&harness.module), range).expect("assigned");
            match sender.try_send(request) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => break,
                Err(TrySendError::Closed(_)) => panic!("packing channel closed while filling"),
            }
        }
    }

    fn drain_packing(harness: &mut Harness) -> Vec<PackingRequest> {
        let mut queued = Vec::new();
        while let Ok(request) = harness.receivers.packing.try_recv() {
            queued.push(request);
        }
        queued
    }

    fn assert_full_partition_span(request: &PackingRequest) {
        assert_eq!(*request.chunk_range().0.start(), 0);
        assert_eq!(*request.chunk_range().0.end(), 9);
    }

    fn repair_one_to_three() -> PartitionChunkRange {
        PartitionChunkRange(irys_types::partition_chunk_offset_ii!(1, 3))
    }

    /// An unassigned module cannot pack. The tick must drop the range.
    /// Putting it back makes the next tick expand and retry it forever.
    #[tokio::test]
    async fn unassigned_module_drops_crc_repairs() {
        let mut harness = harness(Some(DataLedger::Publish.into()));
        let path = harness.path.clone();
        apply_update(
            &mut harness,
            Arc::new(vec![StorageModuleInfo {
                id: 0,
                partition_assignment: None,
                submodules: vec![(partition_chunk_offset_ie!(0, 10), path)],
            }]),
            4,
        )
        .await;
        assert!(harness.module.partition_assignment().is_none());
        harness
            .module
            .requeue_crc_repair_range(repair_one_to_three().0);
        harness.inner.tick();
        assert!(
            harness.receivers.packing.try_recv().is_err(),
            "an unassigned module must not enqueue a repair"
        );
        let left = harness.module.take_crc_repair_ranges();
        assert!(
            left.is_empty(),
            "unassigned module must drop crc repairs, still queued: {left:?}"
        );
        harness.inner.tick();
        let left = harness.module.take_crc_repair_ranges();
        assert!(
            left.is_empty(),
            "a later tick must not restore dropped repairs: {left:?}"
        );
    }

    /// A module that still has an assignment sends one range and does not
    /// leave it queued.
    #[tokio::test]
    async fn assigned_module_enqueues_a_crc_repair() {
        let mut harness = harness(None);
        harness
            .module
            .requeue_crc_repair_range(repair_one_to_three().0);
        harness.inner.tick();
        let request = harness
            .receivers
            .packing
            .try_recv()
            .expect("an assigned module enqueues the repair");
        assert_eq!(*request.chunk_range().0.start(), 1);
        assert_eq!(*request.chunk_range().0.end(), 3);
        assert!(harness.module.take_crc_repair_ranges().is_empty());
        assert!(harness.receivers.packing.try_recv().is_err());
    }

    /// A full channel must not expand the repair into one offset per chunk.
    /// The same range is sent once a slot is free.
    #[tokio::test]
    async fn full_channel_retries_a_crc_repair_as_one_range() {
        let mut harness = harness(None);
        fill_packing_channel(&harness);
        harness
            .module
            .requeue_crc_repair_range(repair_one_to_three().0);
        harness.inner.tick();
        let left = harness.module.take_crc_repair_ranges();
        assert!(
            left.is_empty(),
            "a full channel must keep the repair as one range, not put offsets back: {left:?}"
        );
        assert!(
            harness.receivers.packing.try_recv().is_ok(),
            "the filled channel still holds a filler request"
        );
        harness.inner.tick();
        let mut saw = false;
        for _ in 0..5_001 {
            let Ok(request) = harness.receivers.packing.try_recv() else {
                break;
            };
            if *request.chunk_range().0.start() == 1 && *request.chunk_range().0.end() == 3 {
                saw = true;
                break;
            }
            assert_eq!(
                *request.chunk_range().0.end(),
                0,
                "unexpected packing range {request:?}"
            );
        }
        assert!(saw, "the repair range is sent once the channel has room");
    }

    /// The channel has room. Ledger to capacity sends the reset span now.
    #[tokio::test]
    async fn open_packing_channel_sends_the_reset_pack() {
        let mut harness = harness(Some(DataLedger::Publish.into()));
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        let path = harness.path.clone();
        apply_update(&mut harness, capacity_update(&path, hash, miner), 4).await;
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            None
        );
        let request = harness
            .receivers
            .packing
            .try_recv()
            .expect("an open channel accepts the reset pack");
        assert_full_partition_span(&request);
        assert!(
            harness.inner.pending_ranges.get(&0).is_none(),
            "an accepted reset pack is not retried"
        );
    }

    /// A span remembered for the capacity reset must not run after the
    /// partition is given a ledger slot. That transition does not repack.
    #[tokio::test]
    async fn capacity_to_ledger_does_not_send_a_deferred_reset_pack() {
        let mut harness = harness(Some(DataLedger::Publish.into()));
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        let path = harness.path.clone();
        fill_packing_channel(&harness);
        apply_update(&mut harness, capacity_update(&path, hash, miner), 4).await;
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            None
        );
        apply_update(&mut harness, publish_update(&path, hash, miner), 5).await;
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            Some(DataLedger::Publish.into())
        );
        let _fillers = drain_packing(&mut harness);
        harness.inner.tick();
        assert!(
            harness.receivers.packing.try_recv().is_err(),
            "capacity to ledger must not pack a span deferred from the capacity reset"
        );
    }

    fn assert_only_filler_ranges(queued: &[PackingRequest]) {
        assert!(
            queued
                .iter()
                .all(|request| *request.chunk_range().0.end() == 0),
            "a reset span was accepted while the channel was full"
        );
    }

    fn deferred_span_is_sent_on_tick(harness: &mut Harness) {
        let queued = drain_packing(harness);
        assert_only_filler_ranges(&queued);
        harness.inner.tick();
        let request = harness
            .receivers
            .packing
            .try_recv()
            .expect("the reset span is sent once the channel has room");
        assert_full_partition_span(&request);
        assert!(harness.inner.pending_ranges.get(&0).is_none());
    }

    #[tokio::test]
    async fn full_packing_channel_retries_ledger_to_capacity_pack() {
        let mut harness = harness(Some(DataLedger::Publish.into()));
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        let path = harness.path.clone();
        fill_packing_channel(&harness);
        apply_update(&mut harness, capacity_update(&path, hash, miner), 4).await;
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            None
        );
        deferred_span_is_sent_on_tick(&mut harness);
    }

    #[tokio::test]
    async fn full_packing_channel_retries_ledger_to_ledger_pack() {
        let mut harness = harness(Some(DataLedger::Submit.into()));
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        fill_packing_channel(&harness);
        let update = Arc::new(vec![StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment {
                partition_hash: hash,
                miner_address: miner,
                ledger_id: Some(DataLedger::Publish.into()),
                slot_index: Some(1),
            }),
            submodules: vec![(partition_chunk_offset_ie!(0, 10), harness.path.clone())],
        }]);
        apply_update(&mut harness, update, 8).await;
        let assignment = harness.module.partition_assignment().unwrap();
        assert_eq!(assignment.ledger_id, Some(DataLedger::Publish.into()));
        assert_eq!(assignment.slot_index, Some(1));
        deferred_span_is_sent_on_tick(&mut harness);
    }

    #[tokio::test]
    async fn full_packing_channel_retries_fresh_assignment_pack() {
        let mut harness = harness(None);
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        let path = harness.path.clone();
        fill_packing_channel(&harness);
        apply_update(
            &mut harness,
            Arc::new(vec![StorageModuleInfo {
                id: 0,
                partition_assignment: None,
                submodules: vec![(partition_chunk_offset_ie!(0, 10), path.clone())],
            }]),
            4,
        )
        .await;
        assert!(harness.module.partition_assignment().is_none());
        apply_update(&mut harness, capacity_update(&path, hash, miner), 5).await;
        assert!(harness.module.partition_assignment().is_some());
        deferred_span_is_sent_on_tick(&mut harness);
    }

    /// A later update with the same ledger and slot must not drop a span the
    /// channel has not accepted yet.
    #[tokio::test]
    async fn same_assignment_keeps_a_deferred_reset_pack() {
        let mut harness = harness(Some(DataLedger::Publish.into()));
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        let path = harness.path.clone();
        fill_packing_channel(&harness);
        apply_update(&mut harness, capacity_update(&path, hash, miner), 4).await;
        apply_update(&mut harness, capacity_update(&path, hash, miner), 5).await;
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            None
        );
        deferred_span_is_sent_on_tick(&mut harness);
    }

    /// Unassign drops a span the channel did not accept. The module cannot pack.
    #[tokio::test]
    async fn unassign_drops_a_deferred_reset_pack() {
        let mut harness = harness(Some(DataLedger::Publish.into()));
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        let path = harness.path.clone();
        fill_packing_channel(&harness);
        apply_update(&mut harness, capacity_update(&path, hash, miner), 4).await;
        assert!(harness.inner.pending_ranges.get(&0).is_some());
        apply_update(
            &mut harness,
            Arc::new(vec![StorageModuleInfo {
                id: 0,
                partition_assignment: None,
                submodules: vec![(partition_chunk_offset_ie!(0, 10), path)],
            }]),
            5,
        )
        .await;
        assert!(harness.module.partition_assignment().is_none());
        assert!(
            harness.inner.pending_ranges.get(&0).is_none(),
            "unassign must drop a span the channel did not accept"
        );
        let _fillers = drain_packing(&mut harness);
        harness.inner.tick();
        assert!(harness.receivers.packing.try_recv().is_err());
    }

    #[tokio::test]
    async fn closed_packing_channel_remembers_the_reset_pack() {
        let mut harness = harness(Some(DataLedger::Publish.into()));
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        let path = harness.path.clone();
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        drop(std::mem::replace(&mut harness.receivers.packing, rx));
        apply_update(&mut harness, capacity_update(&path, hash, miner), 4).await;
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            None
        );
        let pending = harness
            .inner
            .pending_ranges
            .get(&0)
            .cloned()
            .unwrap_or_default();
        assert_eq!(
            pending.len(),
            1,
            "a closed channel must remember the reset span"
        );
        assert_eq!(*pending[0].0.start(), 0);
        assert_eq!(*pending[0].0.end(), 9);
        harness.inner.tick();
        let pending = harness
            .inner
            .pending_ranges
            .get(&0)
            .cloned()
            .unwrap_or_default();
        assert_eq!(
            pending.len(),
            1,
            "tick retries a closed channel instead of dropping the span"
        );
    }
}
