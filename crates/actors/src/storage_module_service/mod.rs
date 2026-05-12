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
    /// Rollback marked this span `Uninitialized` and the packing channel did
    /// not accept it. `tick` resends it while any offset is still
    /// `Uninitialized`.
    RepackUninitialized {
        module_id: usize,
        range: PartitionChunkRange,
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
    pending_ranges: HashMap<usize, Vec<PendingSpan>>,
    /// Test stand-in for a periodic heal pass. `None` in every other test.
    #[cfg(test)]
    heal_probe: Option<Arc<HealScheduleProbe>>,
}

/// Counts 1s-arm runs between periodic heal passes. The pass sleeps for
/// `hold` and leaves the retry flag set, so the select order is what the
/// test measures.
#[cfg(test)]
struct HealScheduleProbe {
    hold: Duration,
    passes: std::sync::atomic::AtomicU64,
    /// 1s-arm runs since the previous periodic pass began.
    ticks_since_pass: std::sync::atomic::AtomicU64,
    /// 1s-arm runs after the first periodic pass returned, read at the
    /// start of the second pass.
    ticks_between_passes: std::sync::atomic::AtomicU64,
}

#[cfg(test)]
impl HealScheduleProbe {
    fn record_pass_start(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        let pass = self.passes.fetch_add(1, Relaxed);
        if pass == 1 {
            self.ticks_between_passes
                .store(self.ticks_since_pass.load(Relaxed), Relaxed);
        }
        self.ticks_since_pass.store(0, Relaxed);
    }

    fn record_fast_tick(&self) {
        self.ticks_since_pass
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
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

/// Send one packing request without blocking.
///
/// `Full` and `Closed` are [`PackSend::Deferred`]: the caller keeps the span.
/// An invalid request is [`PackSend::Rejected`] and is not retried.
pub(crate) fn send_packing_request(
    sender: &crate::packing_service::PackingSender,
    module: &Arc<StorageModule>,
    range: PartitionChunkRange,
) -> PackSend {
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
                "Deferring packing request until the channel accepts it"
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

/// Outcome of one non-blocking send to the packing channel.
pub(crate) enum PackSend {
    Accepted,
    /// The channel is full or closed. The caller keeps the range.
    Deferred,
    /// The request is not valid. An unassigned module is not retried.
    Rejected,
}

/// One span `tick` still has to send.
///
/// `while_uninitialized` is a rollback or assignment repack: only offsets
/// that are still `Uninitialized` are sent, and the span is dropped when
/// none remain. A checksum repair is sent whole, including when its label
/// is `Entropy`.
#[derive(Clone, Copy)]
struct PendingSpan {
    range: PartitionChunkRange,
    while_uninitialized: bool,
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
            #[cfg(test)]
            heal_probe: None,
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

    /// Assignment updates go through [`Self::apply_assignment`]. A repack must
    /// not: that path stores the message and would replace a pending reset.
    async fn on_message(&mut self, msg: StorageModuleServiceMessage) -> eyre::Result<()> {
        match msg {
            StorageModuleServiceMessage::RepackUninitialized { module_id, range } => {
                self.defer_uninitialized_repack(module_id, range);
                Ok(())
            }
            other => self.apply_assignment(other).await,
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
            // `on_message` handles this. `apply_assignment` must not see it.
            StorageModuleServiceMessage::RepackUninitialized { .. } => {}
        }
        Ok(())
    }

    /// Try once. A full, closed, or rejected send is kept for `tick` only
    /// while an offset in the span is still `Uninitialized`.
    fn defer_uninitialized_repack(&mut self, module_id: usize, range: PartitionChunkRange) {
        let module = {
            let guard = self.storage_modules.read().unwrap();
            guard.iter().find(|module| module.id == module_id).cloned()
        };
        let Some(module) = module else {
            warn!(
                storage_module.id = module_id,
                "uninitialized repack has no storage module"
            );
            return;
        };
        if module.partition_assignment().is_none() {
            return;
        }
        let mut existing = self.pending_ranges.remove(&module_id).unwrap_or_default();
        let mut added = Vec::new();
        self.enqueue_uninitialized_span(&module, range, &mut added);
        for span in added {
            if !existing
                .iter()
                .any(|kept| kept.while_uninitialized && kept.range == span.range)
            {
                existing.push(span);
            }
        }
        if !existing.is_empty() {
            self.pending_ranges.insert(module_id, existing);
        }
    }

    /// Apply one assignment update. A reset that does not finish is stored and
    /// retried. The service stays up. Any other error still aborts it.
    async fn apply_assignment(&mut self, msg: StorageModuleServiceMessage) -> eyre::Result<()> {
        match self.handle_message(msg.clone()).await {
            Ok(()) => {
                // A wipe that is still open must stay pending. Clearing it
                // here lets a later message that matches memory skip the reset.
                if self.any_reset_incomplete() {
                    debug!(
                        "storage module reset is still incomplete; the assignment update stays pending"
                    );
                    self.pending_assignment = Some(msg);
                } else {
                    self.pending_assignment = None;
                }
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
        // A stored retry is applied by the run loop before this tick. This
        // covers the case where that retry was dropped and no message remains.
        if self.pending_assignment.is_none() {
            self.finish_dropped_resets();
        }
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

        // One module's reset error must not decide the assignment of the
        // others. Remember the failure and keep going. Return it after both
        // passes so this same update is retried. A module already at its
        // target takes neither reset branch on that retry.
        let mut reset_not_finished: Option<eyre::Report> = None;

        for (module_id, module) in local_modules_by_id.iter() {
            match update_info_by_id.get(module_id) {
                None => {
                    // storage module is present locally, not present in the update
                    if let Err(err) = self.clear_assignment_if_outdated(
                        module,
                        update_height,
                        "missing_from_update",
                    ) {
                        if !is_reset_not_finished(&err) {
                            return Err(err);
                        }
                        reset_not_finished.get_or_insert(err);
                    }
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
                        if let Err(err) = self.clear_assignment_if_outdated(
                            module,
                            update_height,
                            "explicitly_unassigned",
                        ) {
                            if !is_reset_not_finished(&err) {
                                return Err(err);
                            }
                            reset_not_finished.get_or_insert(err);
                        }
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
                let range = match self.reset_and_install(existing, Some(assignment), update_height)
                {
                    Ok(range) => range,
                    Err(err) => {
                        reset_not_finished.get_or_insert(err);
                        continue;
                    }
                };
                // The protocol assigns a new partition to capacity for one epoch.
                self.enqueue_packing(existing, range);

                // Skip any further validations for now
                continue;
            }

            // Path equality for every update entry was checked in the pass above.
            let path = &self.submodules_config.submodule_paths[sm_info.id];

            // Validate the in memory storage module against on-disk packing parameters
            if let Some(info_pa) = sm_info.partition_assignment {
                // The on-disk file can disagree with memory. A mismatch, including
                // a missing partition hash, is retried. It does not stop the service.
                if let Err(err) = self.validate_packing_params(existing, path, sm_info.id) {
                    reset_not_finished.get_or_insert(err);
                    continue;
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
                        match self.reset_and_install(existing, Some(info_pa), update_height) {
                            Ok(range) => self.enqueue_packing(existing, range),
                            Err(err) => {
                                reset_not_finished.get_or_insert(err);
                            }
                        }
                    } else {
                        // Capacity→ledger does not reset. Entropy and Data stay
                        // unless a durable marker says the wipe already started.
                        // Clear an in-memory flag that has no marker before the
                        // assign. Otherwise the retry of this same update sees
                        // a matching ledger and finishes a wipe this transition
                        // must not do. A marker is finished first, so both
                        // applies agree with open. That clear publishes
                        // Uninitialized while the partition stays assigned, so
                        // the span is packed. A full channel keeps it. Pack
                        // even when the ledger write fails: the retry must not
                        // be the only send. No marker: send only a span the
                        // channel has not accepted that is still Uninitialized.
                        // Do not pack offsets that are already Entropy or Data.
                        let wiped = if existing.reset_incomplete() {
                            match self.settle_incomplete_reset(existing) {
                                Ok(wiped) => wiped,
                                Err(err) => {
                                    reset_not_finished.get_or_insert(err);
                                    continue;
                                }
                            }
                        } else {
                            None
                        };
                        let stored = existing.assign_partition(info_pa, update_height);
                        if let Some(range) = wiped {
                            self.enqueue_packing(existing, range);
                        }
                        if let Err(err) = stored {
                            // The packing-params write failed. The in-memory
                            // assignment is unchanged. Retry the same update.
                            // Do not stop the service. A span the channel did
                            // not accept stays until a later send.
                            warn!(
                                storage_module.id = existing.id,
                                "capacity-to-ledger assignment was not stored; the update will be retried: {err:#}"
                            );
                            reset_not_finished.get_or_insert(ResetNotFinished.into());
                            continue;
                        }
                        if wiped.is_none() {
                            self.enqueue_deferred_uninitialized(existing);
                        }
                    }
                } else if existing.reset_incomplete() {
                    // The ledger and slot match memory, so the change path
                    // does not run. No marker: open would keep the bytes, so
                    // clear the in-memory flag. A marker: the wipe already
                    // started, so finish it. `reset` does not write the
                    // assignment. The partition is still assigned, so the
                    // span is packed. A full channel keeps it.
                    match self.settle_incomplete_reset(existing) {
                        Ok(Some(range)) if existing.partition_assignment().is_some() => {
                            self.enqueue_packing(existing, range);
                        }
                        Ok(_) => {}
                        Err(err) => {
                            reset_not_finished.get_or_insert(err);
                        }
                    }
                }
            }
        }

        if let Some(err) = reset_not_finished {
            // A later module may already be on its new ledger. Heal did not
            // run. The periodic pass covers that module until this retry
            // completes and heal runs for the settled set.
            self.index_heal_needs_retry = true;
            return Err(err);
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
    /// Does not write the assignment. A caller that must install a new
    /// assignment uses [`Self::reset_and_install`].
    fn finish_reset(&self, module: &Arc<StorageModule>) -> eyre::Result<PartitionChunkRange> {
        self.map_reset(module, module.reset())
    }

    /// An in-memory reset with no marker is not a wipe. A marker is finished.
    ///
    /// `Ok(None)` means the flag was cleared and the bytes stayed.
    /// `Ok(Some(range))` is the span the clear published. A read or finish
    /// error stays pending so the same decision can run again.
    fn settle_incomplete_reset(
        &self,
        module: &Arc<StorageModule>,
    ) -> eyre::Result<Option<PartitionChunkRange>> {
        match module.clear_reset_incomplete_without_marker() {
            Ok(true) => Ok(None),
            Ok(false) => Ok(Some(self.finish_reset(module)?)),
            Err(err) => {
                warn!(
                    storage_module.id = module.id,
                    "could not read the reset marker: {err:#}"
                );
                Err(ResetNotFinished.into())
            }
        }
    }

    fn any_reset_incomplete(&self) -> bool {
        self.storage_modules
            .read()
            .unwrap()
            .iter()
            .any(|module| module.reset_incomplete())
    }

    /// No assignment retry is stored. No marker: clear the in-memory flag and
    /// keep the bytes, which is what open does. A marker: finish the wipe.
    /// An assigned module is packed, ledger or capacity. A full channel keeps
    /// the span. A ledger also schedules index heal. An unassigned module is
    /// not packed.
    fn finish_dropped_resets(&mut self) {
        let modules = {
            let guard = self.storage_modules.read().unwrap();
            guard.clone()
        };
        let mut heal = false;
        for module in modules {
            if !module.reset_incomplete() {
                continue;
            }
            let range = match self.settle_incomplete_reset(&module) {
                Ok(Some(range)) => range,
                Ok(None) => continue,
                Err(err) => {
                    debug!(
                        storage_module.id = module.id,
                        "incomplete reset is still unfinished: {err}"
                    );
                    continue;
                }
            };
            if let Some(assignment) = module.partition_assignment() {
                self.enqueue_packing(&module, range);
                if assignment.ledger_id.is_some() {
                    heal = true;
                }
            }
        }
        if heal {
            self.index_heal_needs_retry = true;
        }
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

    /// Send the deferred spans. A rollback or assignment repack sends only
    /// the part that is still `Uninitialized`. A checksum repair is sent
    /// whole. A full channel keeps the span for `tick`.
    fn enqueue_deferred_uninitialized(&mut self, module: &Arc<StorageModule>) {
        let Some(waiting) = self.pending_ranges.remove(&module.id) else {
            return;
        };
        let mut deferred = Vec::new();
        for span in waiting {
            if span.while_uninitialized {
                self.enqueue_uninitialized_span(module, span.range, &mut deferred);
            } else if matches!(
                self.try_enqueue_packing(module, span.range),
                PackSend::Deferred
            ) {
                deferred.push(span);
            }
        }
        if !deferred.is_empty() {
            self.pending_ranges.insert(module.id, deferred);
        }
    }

    /// Ask the packing service to fill the range. A full or closed channel
    /// does not fail the assignment update: `tick` sends the span later.
    fn enqueue_packing(&mut self, module: &Arc<StorageModule>, range: PartitionChunkRange) {
        // This span replaces any range the channel has not accepted. The
        // reset already cleared the old bytes.
        self.pending_ranges.remove(&module.id);
        if matches!(self.try_enqueue_packing(module, range), PackSend::Deferred) {
            self.pending_ranges.insert(
                module.id,
                vec![PendingSpan {
                    range,
                    while_uninitialized: true,
                }],
            );
        }
    }

    /// No partition assignment is not a warning: packing cannot run.
    fn try_enqueue_packing(
        &self,
        module: &Arc<StorageModule>,
        range: PartitionChunkRange,
    ) -> PackSend {
        send_packing_request(&self.service_senders.packing_sender(), module, range)
    }

    /// Hand checksum mismatches and failed packs to the packing queue. A
    /// full channel keeps each range here, as one interval. An unassigned
    /// module drops both.
    fn enqueue_crc_repairs(&mut self, module: &Arc<StorageModule>) {
        if module.partition_assignment().is_none() {
            // The module cannot pack. Leaving the range queued makes every
            // later tick expand it and try again.
            let _dropped = module.take_crc_repair_ranges();
            let _dropped_packs = module.take_packing_ranges();
            self.pending_ranges.remove(&module.id);
            return;
        }

        let mut waiting = self.pending_ranges.remove(&module.id).unwrap_or_default();
        waiting.extend(
            module
                .take_crc_repair_ranges()
                .into_iter()
                .map(|interval| PendingSpan {
                    range: PartitionChunkRange(interval),
                    while_uninitialized: false,
                }),
        );
        // A failed pack left these spans uninitialized. Resend only the part
        // that still is.
        waiting.extend(
            module
                .take_packing_ranges()
                .into_iter()
                .map(|interval| PendingSpan {
                    range: PartitionChunkRange(interval),
                    while_uninitialized: true,
                }),
        );
        let mut deferred = Vec::new();
        for span in waiting {
            if span.while_uninitialized {
                self.enqueue_uninitialized_span(module, span.range, &mut deferred);
                continue;
            }
            match self.try_enqueue_packing(module, span.range) {
                PackSend::Accepted => {}
                PackSend::Deferred => deferred.push(span),
                PackSend::Rejected => {
                    // `new` failed while the module is still assigned: the
                    // range does not fit the partition. Put that range back.
                    // An assignment cleared during the send is not retried.
                    if module.partition_assignment().is_some() {
                        module.requeue_crc_repair_range(span.range.0);
                    }
                }
            }
        }
        if !deferred.is_empty() {
            self.pending_ranges.insert(module.id, deferred);
        }
    }

    /// Send offsets in `range` that are still `Uninitialized`. A full, closed,
    /// or rejected send stays in `deferred`. No such offset drops the span.
    fn enqueue_uninitialized_span(
        &mut self,
        module: &Arc<StorageModule>,
        range: PartitionChunkRange,
        deferred: &mut Vec<PendingSpan>,
    ) {
        for overlap in module.uninitialized_spans_overlapping(range.0) {
            let range = PartitionChunkRange(overlap);
            let kept = PendingSpan {
                range,
                while_uninitialized: true,
            };
            match self.try_enqueue_packing(module, range) {
                PackSend::Accepted => {}
                PackSend::Deferred => deferred.push(kept),
                PackSend::Rejected => {
                    if module.partition_assignment().is_some() {
                        deferred.push(kept);
                    }
                }
            }
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
            // This update cannot change the assignment. Storing it would pin
            // the retry on a skip. No marker: open would keep the bytes, so
            // clear the in-memory flag. A marker: the wipe already started,
            // so finish it. The partition is still assigned, so the span is
            // packed. A full channel keeps it.
            if module.reset_incomplete()
                && let Some(range) = self.settle_incomplete_reset(module)?
                && module.partition_assignment().is_some()
            {
                self.enqueue_packing(module, range);
            }
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

    /// The in-memory assignment must match `packing_params.toml`.
    /// A mismatch, including a missing partition hash, is [`ResetNotFinished`].
    /// The caller retries that update. A file that cannot be read is skipped.
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

        // A missing disk hash does not match. Unwrapping it would stop the service.
        let hash_match = params
            .partition_hash
            .is_some_and(|hash| hash == assignment.partition_hash);
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

        // Collect detailed mismatch information for the retry log
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

        warn!(
            "Storage module {:?} at index {} has mismatched packing params; the update will be retried: {}",
            &module_path,
            index,
            mismatches.join(", ")
        );
        Err(ResetNotFinished.into())
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

    /// One periodic heal. A test probe sleeps in place of the real pass.
    async fn periodic_index_heal(&mut self) {
        #[cfg(test)]
        if let Some(probe) = self.inner.heal_probe.clone() {
            probe.record_pass_start();
            tokio::time::sleep(probe.hold).await;
            self.inner.index_heal_needs_retry = true;
            return;
        }
        if let Err(e) = self.inner.run_index_heal().await {
            error!("periodic data-index heal failed: {e:?}");
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
        #[cfg(test)]
        if self.inner.heal_probe.is_some() {
            // Startup heal on a clean tree clears the retry flag. The probe
            // measures the periodic arm, so the flag has to stay set.
            self.inner.index_heal_needs_retry = true;
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
                            // Do not drop a stored retry first. apply_assignment
                            // clears it after the wipe finishes, or stores this
                            // message when the wipe is still incomplete.
                            let span = tracing::trace_span!(parent: &parent_span, "storage_module_handle_message");
                            self.inner.on_message(msg).instrument(span).await?;
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
                    let started = tokio::time::Instant::now();
                    self.periodic_index_heal().await;
                    // A pass that lasts one period returns with the next tick
                    // already due. This arm is ahead of the 1s arm, so another
                    // pass would start first. CRC repair and a stored assignment
                    // retry run only on that 1s arm. Wait one period from now.
                    if started.elapsed() >= INDEX_HEAL_RETRY_INTERVAL {
                        heal_interval.reset();
                    }
                }
                _ = interval.tick() => {
                    #[cfg(test)]
                    if let Some(probe) = &self.inner.heal_probe {
                        probe.record_fast_tick();
                    }
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
            let span = tracing::trace_span!(parent: &parent_span, "storage_module_handle_message");
            self.inner.on_message(msg).instrument(span).await?
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
    use irys_database::DatabaseProvider;
    use irys_database::DatabaseProviderTestExt as _;
    use irys_domain::{
        BlockIndex, BlockTree, BlockTreeReadGuard, StorageModule, StorageModuleInfo,
        block_index_guard::BlockIndexReadGuard,
    };
    use irys_testing_utils::{new_mock_signed_header, utils::TempDirBuilder};
    use irys_types::{
        Config, ConsensusConfig, ConsensusOptions, DataLedger, H256, NodeConfig, NodeMode,
        PartitionChunkOffset, ii, partition::PartitionAssignment, partition_chunk_offset_ie,
    };
    use std::{
        sync::{
            Arc, RwLock,
            atomic::{AtomicU64, Ordering},
        },
        time::Duration,
    };
    use tokio::sync::mpsc::error::TrySendError;

    struct Harness {
        _tmp: irys_testing_utils::tempfile::TempDir,
        _shutdown_tx: reth::tasks::shutdown::Signal,
        receivers: crate::services::ServiceReceivers,
        inner: StorageModuleServiceInner,
        module: Arc<StorageModule>,
        path: std::path::PathBuf,
        modules: Vec<Arc<StorageModule>>,
        paths: Vec<std::path::PathBuf>,
    }

    fn harness(ledger_id: Option<u32>) -> Harness {
        open_harness(ledger_id, |_| {})
    }

    fn open_harness(ledger_id: Option<u32>, configure: impl FnOnce(&mut NodeConfig)) -> Harness {
        open_modules(1, ledger_id, configure)
    }

    fn open_modules(
        module_count: usize,
        ledger_id: Option<u32>,
        configure: impl FnOnce(&mut NodeConfig),
    ) -> Harness {
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
        let submodules =
            StorageSubmodulesConfig::load_for_test(tmp.path().to_path_buf(), module_count)
                .expect("submodules");
        let config = Config::new_with_random_peer_id(node_config);
        let miner = config.node_config.miner_address();

        let mut modules = Vec::with_capacity(module_count);
        for id in 0..module_count {
            let path = submodules.submodule_paths[id].clone();
            let info = StorageModuleInfo {
                id,
                partition_assignment: Some(PartitionAssignment {
                    partition_hash: H256::repeat_byte(0x21 + u8::try_from(id).expect("id")),
                    miner_address: miner,
                    ledger_id,
                    slot_index: ledger_id.map(|_| id),
                }),
                submodules: vec![(partition_chunk_offset_ie!(0, 10), path)],
            };
            modules.push(Arc::new(
                StorageModule::new(&info, &config).expect("storage module"),
            ));
        }
        let module = Arc::clone(&modules[0]);
        let paths = submodules.submodule_paths;
        let path = paths[0].clone();

        let db_env = DatabaseProvider::for_testing(
            &(tmp.path().join("chain-db")),
            &(tmp.path().join("chain-db")).join("cache"),
        )
        .expect("chain db");
        let block_index = BlockIndex::new_for_testing(db_env);
        let block_index_guard = BlockIndexReadGuard::new(block_index);
        let block_tree = BlockTree::new(&new_mock_signed_header(), config.consensus.clone());
        let block_tree_guard = BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));
        let (service_senders, receivers) = ServiceSenders::new();
        let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();
        let inner = StorageModuleServiceInner::new(
            Arc::new(RwLock::new(modules.clone())),
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
            modules,
            paths,
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

    fn capacity_assignment(
        id: usize,
        path: &std::path::Path,
        hash: H256,
        miner: irys_types::IrysAddress,
    ) -> StorageModuleInfo {
        StorageModuleInfo {
            id,
            partition_assignment: Some(PartitionAssignment {
                partition_hash: hash,
                miner_address: miner,
                ledger_id: None,
                slot_index: None,
            }),
            submodules: vec![(partition_chunk_offset_ie!(0, 10), path.to_path_buf())],
        }
    }

    /// The first module's reset fails. The later module still moves to the
    /// target. The stored update retries the first module and does not reset
    /// the module that is already there.
    #[tokio::test]
    async fn reset_failure_still_updates_later_modules() {
        use irys_domain::storage_module::ChunkType;

        let mut harness = open_modules(2, Some(DataLedger::Publish.into()), |_| {});
        let first = Arc::clone(&harness.modules[0]);
        let second = Arc::clone(&harness.modules[1]);
        let miner = first
            .partition_assignment()
            .expect("assigned")
            .miner_address;
        let update = Arc::new(vec![
            capacity_assignment(
                0,
                &harness.paths[0],
                first.partition_hash().expect("hash"),
                miner,
            ),
            capacity_assignment(
                1,
                &harness.paths[1],
                second.partition_hash().expect("hash"),
                miner,
            ),
        ]);

        first.fail_next_reset_for_test();
        harness
            .inner
            .apply_assignment(StorageModuleServiceMessage::PartitionAssignmentsUpdated {
                storage_module_infos: Arc::clone(&update),
                update_height: 4,
            })
            .await
            .expect("a reset failure is stored for retry");

        assert_eq!(
            first.partition_assignment().unwrap().ledger_id,
            Some(DataLedger::Publish.into()),
            "the module whose reset failed keeps its assignment"
        );
        assert!(first.reset_incomplete());
        assert_eq!(
            second.partition_assignment().unwrap().ledger_id,
            None,
            "a later module is updated even though the earlier reset failed"
        );
        assert!(!second.reset_incomplete());
        assert!(harness.inner.pending_assignment.is_some());

        let offset = PartitionChunkOffset::from(0);
        assert!(second.write_chunk(offset, vec![0xAB; 32], ChunkType::Entropy));
        second.force_sync_pending_chunks().expect("sync entropy");
        assert_eq!(second.get_chunk_type(&offset), Some(ChunkType::Entropy));

        let queued = drain_packing(&mut harness);
        assert_eq!(queued.len(), 1, "only the module that reset is packed");
        assert_eq!(queued[0].storage_module().id, second.id);
        drop(queued);

        let pending = harness
            .inner
            .pending_assignment
            .clone()
            .expect("retry message");
        harness
            .inner
            .apply_assignment(pending)
            .await
            .expect("retry finishes the first module");

        assert_eq!(first.partition_assignment().unwrap().ledger_id, None);
        assert!(!first.reset_incomplete());
        assert_eq!(second.partition_assignment().unwrap().ledger_id, None);
        assert_eq!(
            second.get_chunk_type(&offset),
            Some(ChunkType::Entropy),
            "a retry must not reset a module that is already at the target"
        );
        assert!(harness.inner.pending_assignment.is_none());

        let queued = drain_packing(&mut harness);
        assert_eq!(
            queued.len(),
            1,
            "the retry packs only the module that reset"
        );
        assert_eq!(queued[0].storage_module().id, first.id);
    }

    /// An unassign whose reset fails must not freeze the other module in the
    /// same update. The retry clears the first module and leaves the second.
    #[tokio::test]
    async fn failed_unassign_still_updates_the_other_module() {
        use irys_domain::storage_module::ChunkType;

        let mut harness = open_modules(2, Some(DataLedger::Publish.into()), |_| {});
        let first = Arc::clone(&harness.modules[0]);
        let second = Arc::clone(&harness.modules[1]);
        let miner = first
            .partition_assignment()
            .expect("assigned")
            .miner_address;
        let update = Arc::new(vec![
            StorageModuleInfo {
                id: 0,
                partition_assignment: None,
                submodules: vec![(partition_chunk_offset_ie!(0, 10), harness.paths[0].clone())],
            },
            capacity_assignment(
                1,
                &harness.paths[1],
                second.partition_hash().expect("hash"),
                miner,
            ),
        ]);

        first.fail_next_reset_for_test();
        harness
            .inner
            .apply_assignment(StorageModuleServiceMessage::PartitionAssignmentsUpdated {
                storage_module_infos: Arc::clone(&update),
                update_height: 4,
            })
            .await
            .expect("a reset failure is stored for retry");

        assert_eq!(
            first.partition_assignment().unwrap().ledger_id,
            Some(DataLedger::Publish.into()),
            "the module whose reset failed keeps its assignment"
        );
        assert!(first.reset_incomplete());
        assert_eq!(
            second.partition_assignment().unwrap().ledger_id,
            None,
            "the other module is updated even though the unassign reset failed"
        );

        let offset = PartitionChunkOffset::from(0);
        assert!(second.write_chunk(offset, vec![0xCD; 32], ChunkType::Entropy));
        second.force_sync_pending_chunks().expect("sync entropy");

        let pending = harness
            .inner
            .pending_assignment
            .clone()
            .expect("retry message");
        harness
            .inner
            .apply_assignment(pending)
            .await
            .expect("retry clears the first module");

        assert!(first.partition_assignment().is_none());
        assert!(!first.reset_incomplete());
        assert_eq!(second.partition_assignment().unwrap().ledger_id, None);
        assert_eq!(
            second.get_chunk_type(&offset),
            Some(ChunkType::Entropy),
            "a retry must not reset a module that is already at the target"
        );
        assert!(harness.inner.pending_assignment.is_none());
    }

    fn write_synced_data(module: &StorageModule, byte: u8) {
        use irys_domain::storage_module::ChunkType;
        let offset = PartitionChunkOffset::from(0);
        assert!(module.write_chunk(offset, vec![byte; 32], ChunkType::Data));
        module.force_sync_pending_chunks().expect("sync data");
        assert_eq!(module.get_chunk_type(&offset), Some(ChunkType::Data));
    }

    fn chunk_type(module: &StorageModule) -> Option<irys_domain::ChunkType> {
        module.get_chunk_type(&PartitionChunkOffset::from(0))
    }

    /// A failed reset set the in-memory flag and wrote no marker. A later
    /// update that names the same ledger and slot must not wipe: open keeps
    /// those bytes. The stored capacity retry is dropped. A second apply, an
    /// idle tick, and a reopen all keep the bytes.
    #[tokio::test]
    async fn matching_assignment_without_a_marker_keeps_the_bytes() {
        use irys_domain::storage_module::ChunkType;

        let mut harness = harness(Some(DataLedger::Publish.into()));
        write_synced_data(&harness.module, 0x11);
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        let path = harness.path.clone();

        harness.module.fail_next_reset_for_test();
        apply_update(&mut harness, capacity_update(&path, hash, miner), 4).await;
        assert!(harness.inner.pending_assignment.is_some());
        assert!(harness.module.reset_incomplete());
        assert!(!reset_marker(&harness.module));
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            Some(DataLedger::Publish.into())
        );
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Data));

        let publish = publish_update(&path, hash, miner);
        apply_update(&mut harness, Arc::clone(&publish), 5).await;
        assert!(
            harness.inner.pending_assignment.is_none(),
            "the capacity retry must not stay pending once the ledger is confirmed"
        );
        let assignment = harness.module.partition_assignment().unwrap();
        assert_eq!(assignment.ledger_id, Some(DataLedger::Publish.into()));
        assert_eq!(assignment.slot_index, Some(0));
        assert!(
            !harness.module.reset_incomplete(),
            "a matching update must clear a flag that has no marker"
        );
        assert!(!reset_marker(&harness.module));
        assert_eq!(
            chunk_type(&harness.module),
            Some(ChunkType::Data),
            "no marker means the wipe never started"
        );
        assert!(harness.receivers.packing.try_recv().is_err());

        apply_update(&mut harness, publish, 5).await;
        harness.inner.tick();
        assert!(!harness.module.reset_incomplete());
        assert!(harness.inner.pending_assignment.is_none());
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Data));
        assert!(harness.receivers.packing.try_recv().is_err());
        assert_reopen_chunk(harness, ChunkType::Data);
    }

    /// The marker is durable, so the same matching update finishes the wipe.
    /// The publish assignment stays. Open would wipe too: the marker is gone
    /// after the finish, and a reopen stays uninitialized.
    #[tokio::test]
    async fn matching_assignment_with_a_durable_marker_finishes_the_wipe() {
        use irys_domain::storage_module::ChunkType;

        let mut harness = harness(Some(DataLedger::Publish.into()));
        write_synced_data(&harness.module, 0x12);
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        let path = harness.path.clone();

        harness.module.fail_next_reset_for_test();
        apply_update(&mut harness, capacity_update(&path, hash, miner), 4).await;
        set_reset_marker(&harness.module);
        assert!(harness.module.reset_incomplete());
        assert!(reset_marker(&harness.module));
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Data));

        let publish = publish_update(&path, hash, miner);
        apply_update(&mut harness, Arc::clone(&publish), 5).await;
        assert!(harness.inner.pending_assignment.is_none());
        let assignment = harness.module.partition_assignment().unwrap();
        assert_eq!(assignment.ledger_id, Some(DataLedger::Publish.into()));
        assert_eq!(assignment.slot_index, Some(0));
        assert!(!harness.module.reset_incomplete());
        assert!(
            !reset_marker(&harness.module),
            "finishing the clear drops the marker"
        );
        assert_eq!(
            chunk_type(&harness.module),
            Some(ChunkType::Uninitialized),
            "a durable marker means the wipe already started"
        );
        let request = harness
            .receivers
            .packing
            .try_recv()
            .expect("an assigned ledger packs the wiped span");
        assert_full_partition_span(&request);
        // The request holds the module. Drop it before reopen takes the db.
        drop(request);
        assert!(
            !harness.inner.index_heal_needs_retry,
            "heal must not keep skipping a reset that has finished"
        );

        apply_update(&mut harness, publish, 5).await;
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Uninitialized));
        assert!(!harness.module.reset_incomplete());
        assert!(
            harness.receivers.packing.try_recv().is_err(),
            "the second apply must not pack the span again"
        );
        assert_reopen_chunk(harness, ChunkType::Uninitialized);
    }

    /// The flag was cleared because no marker existed. A later capacity
    /// assignment is a real ledger move: it still resets and packs.
    #[tokio::test]
    async fn capacity_move_after_a_markerless_flag_is_cleared_still_resets() {
        use irys_domain::storage_module::ChunkType;

        let mut harness = harness(Some(DataLedger::Publish.into()));
        write_synced_data(&harness.module, 0x13);
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        let path = harness.path.clone();

        harness.module.fail_next_reset_for_test();
        apply_update(&mut harness, capacity_update(&path, hash, miner), 4).await;
        apply_update(&mut harness, publish_update(&path, hash, miner), 5).await;
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Data));
        assert!(!harness.module.reset_incomplete());

        apply_update(&mut harness, capacity_update(&path, hash, miner), 6).await;
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            None
        );
        assert!(!harness.module.reset_incomplete());
        assert!(!reset_marker(&harness.module));
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Uninitialized));
        assert!(harness.inner.pending_assignment.is_none());
        let request = harness
            .receivers
            .packing
            .try_recv()
            .expect("a later capacity move still packs");
        assert_full_partition_span(&request);
    }

    /// Replace `packing_params.toml` with bytes `from_toml` rejects.
    /// `validate_packing_params` skips a parse error. `reset` does not read
    /// the file. `store_assignment` does, after the wipe.
    fn corrupt_packing_params(path: &std::path::Path) -> Vec<u8> {
        let file = path.join(irys_domain::PACKING_PARAMS_FILE_NAME);
        let saved = std::fs::read(&file).expect("packing params");
        std::fs::write(&file, b"not-toml").expect("corrupt packing params");
        saved
    }

    fn restore_packing_params(path: &std::path::Path, bytes: &[u8]) {
        std::fs::write(path.join(irys_domain::PACKING_PARAMS_FILE_NAME), bytes)
            .expect("restore packing params");
    }

    /// Ledger to capacity. The wipe commits and the assignment file does not.
    /// A later message that names the old ledger packs that wiped span once.
    #[tokio::test]
    async fn old_ledger_after_failed_assignment_store_packs_the_wipe() {
        use irys_domain::storage_module::ChunkType;

        let mut harness = harness(Some(DataLedger::Publish.into()));
        write_synced_data(&harness.module, 0x51);
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        let path = harness.path.clone();
        let saved = corrupt_packing_params(&path);

        apply_update(&mut harness, capacity_update(&path, hash, miner), 4).await;
        assert!(harness.inner.pending_assignment.is_some());
        assert!(
            harness.module.reset_incomplete(),
            "a failed store must leave the wipe unfinished"
        );
        assert!(
            reset_marker(&harness.module),
            "a failed store must keep the reset marker"
        );
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            Some(DataLedger::Publish.into())
        );
        assert_eq!(
            harness.module.partition_assignment().unwrap().slot_index,
            Some(0)
        );
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Uninitialized));
        assert!(harness.receivers.packing.try_recv().is_err());
        harness.inner.tick();
        assert!(
            harness.receivers.packing.try_recv().is_err(),
            "a stored retry is applied by the run loop, not by this tick"
        );
        assert!(harness.module.reset_incomplete());

        restore_packing_params(&path, &saved);
        let publish = publish_update(&path, hash, miner);
        apply_update(&mut harness, Arc::clone(&publish), 5).await;
        assert!(harness.inner.pending_assignment.is_none());
        assert!(!harness.module.reset_incomplete());
        assert!(!reset_marker(&harness.module));
        let assignment = harness.module.partition_assignment().unwrap();
        assert_eq!(assignment.ledger_id, Some(DataLedger::Publish.into()));
        assert_eq!(assignment.slot_index, Some(0));
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Uninitialized));
        let request = harness
            .receivers
            .packing
            .try_recv()
            .expect("the old ledger packs the wiped span");
        assert_full_partition_span(&request);
        drop(request);
        apply_update(&mut harness, publish, 5).await;
        harness.inner.tick();
        assert!(
            harness.receivers.packing.try_recv().is_err(),
            "the span is packed once"
        );
    }

    /// The same capacity message, retried after the file is readable, still
    /// installs capacity and packs the wipe once.
    #[tokio::test]
    async fn retried_assignment_after_failed_store_packs_once() {
        use irys_domain::storage_module::ChunkType;

        let mut harness = harness(Some(DataLedger::Publish.into()));
        write_synced_data(&harness.module, 0x52);
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        let path = harness.path.clone();
        let saved = corrupt_packing_params(&path);
        let update = capacity_update(&path, hash, miner);
        apply_update(&mut harness, Arc::clone(&update), 4).await;
        assert!(harness.module.reset_incomplete());
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Uninitialized));
        assert!(harness.receivers.packing.try_recv().is_err());

        restore_packing_params(&path, &saved);
        apply_update(&mut harness, update, 4).await;
        assert!(harness.inner.pending_assignment.is_none());
        assert!(!harness.module.reset_incomplete());
        assert!(!reset_marker(&harness.module));
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            None
        );
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Uninitialized));
        let request = harness
            .receivers
            .packing
            .try_recv()
            .expect("the retried capacity assignment packs the wipe");
        assert_full_partition_span(&request);
        assert!(harness.receivers.packing.try_recv().is_err());
    }

    /// The capacity retry was dropped. The tick finishes the marker and packs
    /// one span. It does not pack while that retry is still stored.
    #[tokio::test]
    async fn dropped_retry_after_failed_store_packs_on_tick() {
        use irys_domain::storage_module::ChunkType;

        let mut harness = harness(Some(DataLedger::Publish.into()));
        write_synced_data(&harness.module, 0x53);
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        let path = harness.path.clone();
        let _saved = corrupt_packing_params(&path);
        apply_update(&mut harness, capacity_update(&path, hash, miner), 4).await;
        assert!(harness.inner.pending_assignment.is_some());
        harness.inner.tick();
        assert!(harness.receivers.packing.try_recv().is_err());
        assert!(harness.module.reset_incomplete());

        harness.inner.pending_assignment = None;
        harness.inner.tick();
        assert!(!harness.module.reset_incomplete());
        assert!(!reset_marker(&harness.module));
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            Some(DataLedger::Publish.into()),
            "the tick does not install the dropped capacity assignment"
        );
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Uninitialized));
        let request = harness
            .receivers
            .packing
            .try_recv()
            .expect("a dropped retry packs the wiped span");
        assert_full_partition_span(&request);
        assert!(harness.receivers.packing.try_recv().is_err());
    }

    /// The old ledger confirms the wipe while the packing channel is full.
    /// One span is kept, not one request per chunk, and a later tick sends it.
    #[tokio::test]
    async fn full_channel_defers_one_span_after_failed_store() {
        use irys_domain::storage_module::ChunkType;

        let mut harness = harness(Some(DataLedger::Publish.into()));
        write_synced_data(&harness.module, 0x54);
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        let path = harness.path.clone();
        let saved = corrupt_packing_params(&path);
        apply_update(&mut harness, capacity_update(&path, hash, miner), 4).await;
        assert!(reset_marker(&harness.module));
        restore_packing_params(&path, &saved);
        fill_packing_channel(&harness);
        apply_update(&mut harness, publish_update(&path, hash, miner), 5).await;
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Uninitialized));
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            Some(DataLedger::Publish.into())
        );
        assert!(!harness.module.reset_incomplete());
        assert_one_deferred_full_span(&harness);
        deferred_span_is_sent_on_tick(&mut harness);
        assert!(harness.receivers.packing.try_recv().is_err());
    }

    /// The marker is durable and the finish fails before the clear. The
    /// matching update stays pending, the marker stays, and the bytes stay
    /// until a later attempt completes the wipe.
    #[tokio::test]
    async fn matching_assignment_finish_failure_keeps_the_retry() {
        use irys_domain::storage_module::ChunkType;

        let mut harness = harness(Some(DataLedger::Publish.into()));
        write_synced_data(&harness.module, 0x22);
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        let path = harness.path.clone();

        harness.module.fail_next_reset_for_test();
        apply_update(&mut harness, capacity_update(&path, hash, miner), 4).await;
        set_reset_marker(&harness.module);
        harness.module.fail_next_reset_for_test();
        let publish = publish_update(&path, hash, miner);
        apply_update(&mut harness, Arc::clone(&publish), 5).await;
        assert!(
            harness.inner.pending_assignment.is_some(),
            "the matching update must stay pending until the wipe finishes"
        );
        assert!(harness.module.reset_incomplete());
        assert!(
            reset_marker(&harness.module),
            "a failed finish leaves the marker"
        );
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            Some(DataLedger::Publish.into())
        );
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Data));
        assert!(harness.receivers.packing.try_recv().is_err());

        apply_update(&mut harness, publish, 5).await;
        assert!(harness.inner.pending_assignment.is_none());
        assert!(!harness.module.reset_incomplete());
        assert!(!reset_marker(&harness.module));
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Uninitialized));
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            Some(DataLedger::Publish.into())
        );
        let request = harness
            .receivers
            .packing
            .try_recv()
            .expect("an assigned ledger packs the wiped span");
        assert_full_partition_span(&request);
    }

    /// The retry was dropped and no marker was written. The tick clears the
    /// flag and keeps the bytes. A stored retry is left alone: the tick must
    /// not settle while that update is still waiting. A reopen keeps the bytes.
    #[tokio::test]
    async fn tick_without_a_marker_keeps_ledger_bytes_when_the_retry_was_dropped() {
        use irys_domain::storage_module::ChunkType;

        let mut harness = harness(Some(DataLedger::Publish.into()));
        write_synced_data(&harness.module, 0x33);
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        let path = harness.path.clone();

        harness.module.fail_next_reset_for_test();
        apply_update(&mut harness, capacity_update(&path, hash, miner), 4).await;
        assert!(harness.inner.pending_assignment.is_some());
        harness.inner.tick();
        assert!(
            harness.module.reset_incomplete(),
            "a stored capacity retry must finish on apply, not on an idle tick"
        );
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Data));
        assert!(!reset_marker(&harness.module));

        harness.inner.pending_assignment = None;
        harness.inner.index_heal_needs_retry = false;
        harness.inner.tick();
        assert!(
            !harness.module.reset_incomplete(),
            "a tick with no stored retry clears a flag that has no marker"
        );
        assert_eq!(
            chunk_type(&harness.module),
            Some(ChunkType::Data),
            "no marker means the wipe never started"
        );
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            Some(DataLedger::Publish.into())
        );
        assert_eq!(
            harness.module.partition_assignment().unwrap().slot_index,
            Some(0)
        );
        assert!(
            !harness.inner.index_heal_needs_retry,
            "keeping the bytes must not schedule heal for a cleared index"
        );
        assert!(harness.receivers.packing.try_recv().is_err());
        harness.inner.tick();
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Data));
        assert_reopen_chunk(harness, ChunkType::Data);
    }

    /// The retry was dropped and the marker is durable. The tick finishes the
    /// wipe. It does not finish while the retry is still stored. A failed
    /// finish stays pending for the next tick. A reopen stays uninitialized.
    #[tokio::test]
    async fn tick_finishes_a_dropped_ledger_reset_when_the_marker_is_durable() {
        use irys_domain::storage_module::ChunkType;

        let mut harness = harness(Some(DataLedger::Publish.into()));
        write_synced_data(&harness.module, 0x34);
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        let path = harness.path.clone();

        harness.module.fail_next_reset_for_test();
        apply_update(&mut harness, capacity_update(&path, hash, miner), 4).await;
        set_reset_marker(&harness.module);
        harness.inner.tick();
        assert!(
            harness.module.reset_incomplete(),
            "a stored capacity retry must finish on apply, not on an idle tick"
        );
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Data));
        assert!(reset_marker(&harness.module));

        harness.inner.pending_assignment = None;
        harness.inner.index_heal_needs_retry = false;
        harness.module.fail_next_reset_for_test();
        harness.inner.tick();
        assert!(
            harness.module.reset_incomplete(),
            "a failed finish stays incomplete for the next tick"
        );
        assert!(reset_marker(&harness.module));
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Data));
        assert!(harness.receivers.packing.try_recv().is_err());

        harness.inner.tick();
        assert!(!harness.module.reset_incomplete());
        assert!(!reset_marker(&harness.module));
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Uninitialized));
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            Some(DataLedger::Publish.into())
        );
        assert_eq!(
            harness.module.partition_assignment().unwrap().slot_index,
            Some(0)
        );
        assert!(
            harness.inner.index_heal_needs_retry,
            "heal must run once the wipe has finished and no assignment message will"
        );
        let request = harness
            .receivers
            .packing
            .try_recv()
            .expect("an assigned ledger packs the wiped span");
        assert_full_partition_span(&request);
        // The request holds the module. Drop it before reopen takes the db.
        drop(request);
        assert_reopen_chunk(harness, ChunkType::Uninitialized);
    }

    /// Same dropped retry on capacity, with no marker. The tick keeps the
    /// entropy and does not pack.
    #[tokio::test]
    async fn tick_without_a_marker_keeps_capacity_bytes() {
        use irys_domain::storage_module::ChunkType;

        let mut harness = harness(None);
        write_synced_entropy(&harness.module);
        let path = harness.path.clone();
        harness.module.fail_next_reset_for_test();
        apply_update(&mut harness, explicit_unassign(&path), 4).await;
        assert!(harness.module.reset_incomplete());
        assert!(!reset_marker(&harness.module));
        harness.inner.tick();
        assert!(
            harness.module.reset_incomplete(),
            "a stored unassign must finish on apply, not on an idle tick"
        );
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Entropy));

        harness.inner.pending_assignment = None;
        harness.inner.tick();
        assert!(!harness.module.reset_incomplete());
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Entropy));
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            None
        );
        assert!(harness.receivers.packing.try_recv().is_err());
        assert_reopen_chunk(harness, ChunkType::Entropy);
    }

    /// Same dropped retry on capacity. The marker is durable, so the tick
    /// finishes the wipe and packs.
    #[tokio::test]
    async fn tick_finishes_a_dropped_capacity_reset_and_packs() {
        use irys_domain::storage_module::ChunkType;

        let mut harness = harness(None);
        write_synced_entropy(&harness.module);
        let path = harness.path.clone();
        harness.module.fail_next_reset_for_test();
        apply_update(&mut harness, explicit_unassign(&path), 4).await;
        set_reset_marker(&harness.module);
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Entropy));
        harness.inner.pending_assignment = None;
        harness.inner.index_heal_needs_retry = false;
        harness.inner.tick();
        assert!(!harness.module.reset_incomplete());
        assert!(!reset_marker(&harness.module));
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Uninitialized));
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            None
        );
        assert!(
            !harness.inner.index_heal_needs_retry,
            "capacity has no ledger index for heal"
        );
        let request = harness
            .receivers
            .packing
            .try_recv()
            .expect("a dropped capacity reset is packed from the tick");
        assert_full_partition_span(&request);
    }

    /// A failed unassign set the flag and wrote no marker. A later update that
    /// names the same capacity assignment keeps the entropy and does not pack.
    #[tokio::test]
    async fn matching_capacity_without_a_marker_keeps_entropy() {
        use irys_domain::storage_module::ChunkType;

        let mut harness = harness(None);
        write_synced_entropy(&harness.module);
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        let path = harness.path.clone();
        harness.module.fail_next_reset_for_test();
        apply_update(&mut harness, explicit_unassign(&path), 4).await;
        assert!(harness.inner.pending_assignment.is_some());
        assert!(harness.module.reset_incomplete());
        assert!(!reset_marker(&harness.module));
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Entropy));
        assert!(harness.receivers.packing.try_recv().is_err());

        let capacity = capacity_update(&path, hash, miner);
        apply_update(&mut harness, Arc::clone(&capacity), 5).await;
        assert!(
            harness.inner.pending_assignment.is_none(),
            "the unassign retry must not stay pending once capacity is confirmed"
        );
        assert!(!harness.module.reset_incomplete());
        assert!(!reset_marker(&harness.module));
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            None
        );
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Entropy));
        assert!(harness.receivers.packing.try_recv().is_err());

        apply_update(&mut harness, capacity, 5).await;
        harness.inner.tick();
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Entropy));
        assert!(!harness.module.reset_incomplete());
        assert!(harness.receivers.packing.try_recv().is_err());
        assert_reopen_chunk(harness, ChunkType::Entropy);
    }

    /// The marker is durable. A later update that names the same capacity
    /// assignment finishes the wipe and packs it.
    #[tokio::test]
    async fn matching_capacity_after_failed_unassign_finishes_and_packs() {
        use irys_domain::storage_module::ChunkType;

        let mut harness = harness(None);
        write_synced_entropy(&harness.module);
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        let path = harness.path.clone();
        harness.module.fail_next_reset_for_test();
        apply_update(&mut harness, explicit_unassign(&path), 4).await;
        set_reset_marker(&harness.module);
        assert!(harness.inner.pending_assignment.is_some());
        assert!(harness.module.reset_incomplete());
        assert!(reset_marker(&harness.module));
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Entropy));
        assert!(harness.receivers.packing.try_recv().is_err());

        apply_update(&mut harness, capacity_update(&path, hash, miner), 5).await;
        assert!(harness.inner.pending_assignment.is_none());
        assert!(!harness.module.reset_incomplete());
        assert!(!reset_marker(&harness.module));
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            None
        );
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Uninitialized));
        let request = harness
            .receivers
            .packing
            .try_recv()
            .expect("capacity is packed after the wipe");
        assert_full_partition_span(&request);
    }

    /// A repeated assignment with no incomplete reset is not a wipe.
    #[tokio::test]
    async fn unchanged_assignment_without_an_incomplete_reset_keeps_its_bytes() {
        use irys_domain::storage_module::ChunkType;

        let mut harness = harness(Some(DataLedger::Publish.into()));
        write_synced_data(&harness.module, 0x44);
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        let path = harness.path.clone();
        harness
            .inner
            .apply_assignment(StorageModuleServiceMessage::PartitionAssignmentsUpdated {
                storage_module_infos: publish_update(&path, hash, miner),
                update_height: 6,
            })
            .await
            .expect("same ledger and slot");
        assert!(!harness.module.reset_incomplete());
        assert!(harness.inner.pending_assignment.is_none());
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Data));
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            Some(DataLedger::Publish.into())
        );
        assert!(harness.receivers.packing.try_recv().is_err());
    }

    /// A failed reset then a different slot still installs that slot and packs it.
    #[tokio::test]
    async fn failed_reset_then_a_different_slot_still_installs() {
        use irys_domain::storage_module::ChunkType;

        let mut harness = harness(Some(DataLedger::Submit.into()));
        write_synced_data(&harness.module, 0x55);
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
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

        harness.module.fail_next_reset_for_test();
        harness
            .inner
            .apply_assignment(StorageModuleServiceMessage::PartitionAssignmentsUpdated {
                storage_module_infos: Arc::clone(&update),
                update_height: 8,
            })
            .await
            .expect("reset failure is retried");
        assert!(harness.inner.pending_assignment.is_some());
        assert!(harness.module.reset_incomplete());
        assert_eq!(
            harness.module.partition_assignment().unwrap().slot_index,
            Some(0)
        );
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Data));

        harness
            .inner
            .apply_assignment(StorageModuleServiceMessage::PartitionAssignmentsUpdated {
                storage_module_infos: update,
                update_height: 8,
            })
            .await
            .expect("the different slot is installed");
        let assignment = harness.module.partition_assignment().unwrap();
        assert_eq!(assignment.ledger_id, Some(DataLedger::Publish.into()));
        assert_eq!(assignment.slot_index, Some(1));
        assert!(!harness.module.reset_incomplete());
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Uninitialized));
        assert!(harness.inner.pending_assignment.is_none());
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

    /// Expiry can store capacity in `packing_params.toml` while memory still
    /// names the ledger. That disagreement is retried. It does not stop the
    /// service. A later call with a file that matches memory applies the update.
    #[tokio::test]
    async fn packing_params_mismatch_is_retried() {
        let mut harness = harness(Some(DataLedger::Publish.into()));
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        let path = harness.path.clone();
        let params_path = path.join(PACKING_PARAMS_FILE_NAME);
        let update = capacity_update(&path, hash, miner);

        let mut params = PackingParams::from_toml(&params_path).expect("packing params");
        params.ledger = None;
        params.slot = None;
        params
            .write_to_disk(&params_path)
            .expect("store capacity on disk");

        let err = harness
            .inner
            .handle_partition_assignments_update(Arc::clone(&update), 4)
            .await
            .expect_err("a ledger mismatch is retried");
        assert!(super::is_reset_not_finished(&err));
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            Some(DataLedger::Publish.into()),
            "a mismatch must leave the in-memory assignment unchanged"
        );

        params.ledger = Some(DataLedger::Publish.into());
        params.slot = Some(0);
        params
            .write_to_disk(&params_path)
            .expect("restore the ledger file");

        harness
            .inner
            .handle_partition_assignments_update(update, 4)
            .await
            .expect("a consistent file applies the update");
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            None
        );
        assert!(harness.receivers.packing.try_recv().is_ok());
    }

    /// A disk file with no partition hash is retried, same as a mismatch.
    /// A later call with a hash in the file applies the update.
    #[tokio::test]
    async fn packing_params_without_a_partition_hash_is_retried() {
        let mut harness = harness(Some(DataLedger::Publish.into()));
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        let path = harness.path.clone();
        let params_path = path.join(PACKING_PARAMS_FILE_NAME);
        let update = capacity_update(&path, hash, miner);

        let mut params = PackingParams::from_toml(&params_path).expect("packing params");
        params.partition_hash = None;
        params
            .write_to_disk(&params_path)
            .expect("clear the disk hash");

        let err = harness
            .inner
            .handle_partition_assignments_update(Arc::clone(&update), 4)
            .await
            .expect_err("a missing disk hash is retried");
        assert!(super::is_reset_not_finished(&err));
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            Some(DataLedger::Publish.into())
        );
        assert_eq!(
            harness
                .module
                .partition_assignment()
                .unwrap()
                .partition_hash,
            hash
        );

        params.partition_hash = Some(hash);
        params
            .write_to_disk(&params_path)
            .expect("restore the disk hash");

        harness
            .inner
            .handle_partition_assignments_update(update, 4)
            .await
            .expect("a file with a hash applies the update");
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            None
        );
    }

    /// Module 0 changes ledger. Module 1's packing file disagrees with memory.
    /// Module 0 still commits. The call returns the retryable error and arms
    /// the heal retry. A later call with a file that matches memory updates
    /// module 1.
    #[tokio::test]
    async fn packing_params_mismatch_still_updates_the_other_module() {
        let mut harness = open_modules(2, Some(DataLedger::Publish.into()), |_| {});
        let first = Arc::clone(&harness.modules[0]);
        let second = Arc::clone(&harness.modules[1]);
        let miner = first
            .partition_assignment()
            .expect("assigned")
            .miner_address;
        let update = Arc::new(vec![
            capacity_assignment(
                0,
                &harness.paths[0],
                first.partition_hash().expect("hash"),
                miner,
            ),
            capacity_assignment(
                1,
                &harness.paths[1],
                second.partition_hash().expect("hash"),
                miner,
            ),
        ]);

        let params_path = harness.paths[1].join(PACKING_PARAMS_FILE_NAME);
        let mut params = PackingParams::from_toml(&params_path).expect("packing params");
        let saved = params;
        params.ledger = None;
        params.slot = None;
        params
            .write_to_disk(&params_path)
            .expect("store a disagreeing file");

        harness.inner.index_heal_needs_retry = false;
        let err = harness
            .inner
            .handle_partition_assignments_update(Arc::clone(&update), 4)
            .await
            .expect_err("a packing-params mismatch is retried");
        assert!(super::is_reset_not_finished(&err));
        assert_eq!(
            first.partition_assignment().unwrap().ledger_id,
            None,
            "the earlier module's ledger change commits"
        );
        assert_eq!(
            second.partition_assignment().unwrap().ledger_id,
            Some(DataLedger::Publish.into()),
            "a disagreeing file is not applied"
        );
        assert!(
            harness.inner.index_heal_needs_retry,
            "a retryable mismatch must still arm index heal"
        );

        saved
            .write_to_disk(&params_path)
            .expect("restore a file that matches memory");
        harness
            .inner
            .handle_partition_assignments_update(update, 4)
            .await
            .expect("a consistent file updates the later module");
        assert_eq!(
            second.partition_assignment().unwrap().ledger_id,
            None,
            "the retry applies the later module"
        );
    }

    fn reset_marker(module: &StorageModule) -> bool {
        use irys_database::submodule::SubmoduleStore as _;
        let (_, submodule) = module
            .get_submodule_for_offset(PartitionChunkOffset::from(0))
            .expect("submodule");
        submodule.db.reset_in_progress().expect("read reset marker")
    }

    fn set_reset_marker(module: &StorageModule) {
        use irys_database::submodule::SubmoduleStore as _;
        let (_, submodule) = module
            .get_submodule_for_offset(PartitionChunkOffset::from(0))
            .expect("submodule");
        submodule
            .db
            .set_reset_in_progress()
            .expect("set reset marker");
    }

    fn explicit_unassign(path: &std::path::Path) -> Arc<Vec<StorageModuleInfo>> {
        Arc::new(vec![StorageModuleInfo {
            id: 0,
            partition_assignment: None,
            submodules: vec![(partition_chunk_offset_ie!(0, 10), path.to_path_buf())],
        }])
    }

    fn publish_slot_update(
        path: &std::path::Path,
        hash: H256,
        miner: irys_types::IrysAddress,
        slot: usize,
    ) -> Arc<Vec<StorageModuleInfo>> {
        Arc::new(vec![StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment {
                partition_hash: hash,
                miner_address: miner,
                ledger_id: Some(DataLedger::Publish.into()),
                slot_index: Some(slot),
            }),
            submodules: vec![(partition_chunk_offset_ie!(0, 10), path.to_path_buf())],
        }])
    }

    fn stored_height(path: &std::path::Path) -> Option<u64> {
        irys_domain::PackingParams::from_toml(path.join(irys_domain::PACKING_PARAMS_FILE_NAME))
            .expect("packing params")
            .last_updated_height
    }

    /// Slot 1 is stored at `height`, then one `Data` byte is synced over it.
    async fn store_slot_one_with_data(
        height: u64,
        byte: u8,
    ) -> (Harness, H256, irys_types::IrysAddress) {
        let mut harness = harness(Some(DataLedger::Publish.into()));
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        let path = harness.path.clone();
        apply_update(
            &mut harness,
            publish_slot_update(&path, hash, miner, 1),
            height,
        )
        .await;
        assert_eq!(stored_height(&harness.path), Some(height));
        assert_eq!(
            harness.module.partition_assignment().unwrap().slot_index,
            Some(1)
        );
        write_synced_data(&harness.module, byte);
        let _packed = drain_packing(&mut harness);
        (harness, hash, miner)
    }

    /// A lower explicit unassign must not stay pending. No marker was written,
    /// so the bytes and the slot stay, and the in-memory flag is cleared.
    /// A later reapply and tick must not wipe what open would keep.
    #[tokio::test]
    async fn stale_unassign_does_not_pin_an_incomplete_reset() {
        use irys_domain::storage_module::ChunkType;

        let (mut harness, hash, miner) = store_slot_one_with_data(50, 0xAB).await;
        let path = harness.path.clone();
        harness.module.fail_next_reset_for_test();
        apply_update(&mut harness, capacity_update(&path, hash, miner), 60).await;
        assert!(harness.module.reset_incomplete());
        assert!(!reset_marker(&harness.module));
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Data));
        assert_eq!(
            harness.module.partition_assignment().unwrap().slot_index,
            Some(1)
        );

        let unassign = explicit_unassign(&path);
        apply_update(&mut harness, Arc::clone(&unassign), 10).await;
        assert!(
            !harness.module.reset_incomplete(),
            "a skipped unassign must clear an in-memory flag that has no marker"
        );
        assert!(
            harness.inner.pending_assignment.is_none(),
            "a skip must not become the stored retry"
        );
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Data));
        assert_eq!(
            harness.module.partition_assignment().unwrap().slot_index,
            Some(1)
        );
        assert!(!reset_marker(&harness.module));
        assert_eq!(stored_height(&path), Some(50));
        assert!(harness.receivers.packing.try_recv().is_err());

        apply_update(&mut harness, unassign, 10).await;
        harness.inner.tick();
        assert!(!harness.module.reset_incomplete());
        assert!(harness.inner.pending_assignment.is_none());
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Data));
        assert_eq!(
            harness.module.partition_assignment().unwrap().slot_index,
            Some(1)
        );
        assert!(harness.receivers.packing.try_recv().is_err());
    }

    /// The marker is durable. The same lower unassign finishes the clear and
    /// does not stay pending. The assignment is the newer local slot.
    #[tokio::test]
    async fn stale_unassign_finishes_a_durable_reset() {
        use irys_domain::storage_module::ChunkType;

        let (mut harness, hash, miner) = store_slot_one_with_data(50, 0xCD).await;
        let path = harness.path.clone();
        harness.module.fail_next_reset_for_test();
        apply_update(&mut harness, capacity_update(&path, hash, miner), 60).await;
        set_reset_marker(&harness.module);
        assert!(harness.module.reset_incomplete());
        assert!(reset_marker(&harness.module));
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Data));

        apply_update(&mut harness, explicit_unassign(&path), 10).await;
        assert!(
            !harness.module.reset_incomplete(),
            "a durable marker must be finished, not left pinned"
        );
        assert!(harness.inner.pending_assignment.is_none());
        assert!(
            !reset_marker(&harness.module),
            "finishing the clear drops the marker"
        );
        assert_eq!(
            chunk_type(&harness.module),
            Some(ChunkType::Uninitialized),
            "a durable marker means the wipe already started"
        );
        assert_eq!(
            harness.module.partition_assignment().unwrap().slot_index,
            Some(1),
            "the lower unassign does not replace the newer assignment"
        );
        assert_eq!(stored_height(&path), Some(50));
        let request = harness
            .receivers
            .packing
            .try_recv()
            .expect("an assigned ledger packs the wiped span");
        assert_full_partition_span(&request);
    }

    /// The finish can fail. The lower unassign stays pending only until the
    /// clear completes. It must not skip forever.
    #[tokio::test]
    async fn stale_unassign_retries_a_durable_reset_until_it_finishes() {
        use irys_domain::storage_module::ChunkType;

        let (mut harness, hash, miner) = store_slot_one_with_data(50, 0xEF).await;
        let path = harness.path.clone();
        harness.module.fail_next_reset_for_test();
        apply_update(&mut harness, capacity_update(&path, hash, miner), 60).await;
        set_reset_marker(&harness.module);
        harness.module.fail_next_reset_for_test();

        let unassign = explicit_unassign(&path);
        apply_update(&mut harness, Arc::clone(&unassign), 10).await;
        assert!(
            harness.inner.pending_assignment.is_some(),
            "a failed finish of a durable marker stays pending"
        );
        assert!(harness.module.reset_incomplete());
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Data));
        assert_eq!(
            harness.module.partition_assignment().unwrap().slot_index,
            Some(1)
        );
        assert!(
            harness.receivers.packing.try_recv().is_err(),
            "a failed finish has not published the wipe"
        );

        apply_update(&mut harness, unassign, 10).await;
        assert!(harness.inner.pending_assignment.is_none());
        assert!(!harness.module.reset_incomplete());
        assert!(!reset_marker(&harness.module));
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Uninitialized));
        assert_eq!(
            harness.module.partition_assignment().unwrap().slot_index,
            Some(1)
        );
        let request = harness
            .receivers
            .packing
            .try_recv()
            .expect("an assigned ledger packs the wiped span once the finish succeeds");
        assert_full_partition_span(&request);
    }

    /// The same lower unassign on a capacity module whose wipe already started
    /// finishes the clear and packs. Capacity has no ledger for heal.
    #[tokio::test]
    async fn stale_unassign_finishes_a_durable_capacity_reset_and_packs() {
        use irys_domain::storage_module::ChunkType;

        let (mut harness, hash, miner) = store_slot_one_with_data(50, 0x11).await;
        let path = harness.path.clone();
        apply_update(&mut harness, capacity_update(&path, hash, miner), 50).await;
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            None
        );
        assert_eq!(stored_height(&path), Some(50));
        let _packed = drain_packing(&mut harness);
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
        harness.module.fail_next_reset_for_test();
        apply_update(&mut harness, explicit_unassign(&path), 60).await;
        set_reset_marker(&harness.module);
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Entropy));

        apply_update(&mut harness, explicit_unassign(&path), 10).await;
        assert!(!harness.module.reset_incomplete());
        assert!(harness.inner.pending_assignment.is_none());
        assert!(!reset_marker(&harness.module));
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Uninitialized));
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            None
        );
        let request = harness
            .receivers
            .packing
            .try_recv()
            .expect("finished capacity wipe is packed");
        assert_eq!(*request.chunk_range().0.start(), 0);
        assert_eq!(*request.chunk_range().0.end(), 9);
    }

    /// A lower unassign with no incomplete reset still skips. Bytes stay.
    #[tokio::test]
    async fn stale_unassign_without_an_incomplete_reset_keeps_its_bytes() {
        use irys_domain::storage_module::ChunkType;

        let (mut harness, _hash, _miner) = store_slot_one_with_data(50, 0x22).await;
        let path = harness.path.clone();
        apply_update(&mut harness, explicit_unassign(&path), 10).await;
        assert!(!harness.module.reset_incomplete());
        assert!(harness.inner.pending_assignment.is_none());
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Data));
        assert_eq!(
            harness.module.partition_assignment().unwrap().slot_index,
            Some(1)
        );
        assert_eq!(stored_height(&path), Some(50));
        assert!(harness.receivers.packing.try_recv().is_err());
    }

    /// An unassign at the stored height is not stale. It still clears and
    /// does not pack an unassigned module.
    #[tokio::test]
    async fn unassign_at_the_stored_height_still_resets() {
        use irys_domain::storage_module::ChunkType;

        let (mut harness, _hash, _miner) = store_slot_one_with_data(50, 0x33).await;
        let path = harness.path.clone();
        apply_update(&mut harness, explicit_unassign(&path), 50).await;
        assert!(harness.module.partition_assignment().is_none());
        assert!(!harness.module.reset_incomplete());
        assert!(harness.inner.pending_assignment.is_none());
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Uninitialized));
        assert!(harness.receivers.packing.try_recv().is_err());
    }

    fn write_synced_entropy(module: &StorageModule) {
        use irys_domain::storage_module::ChunkType;
        let offset = PartitionChunkOffset::from(0);
        assert!(module.write_chunk(offset, vec![0x5A_u8; 32], ChunkType::Entropy));
        module
            .force_sync_pending_chunks()
            .expect("entropy is durable");
        assert_eq!(module.get_chunk_type(&offset), Some(ChunkType::Entropy));
    }

    fn close_for_reopen(
        harness: Harness,
    ) -> (
        irys_testing_utils::tempfile::TempDir,
        Config,
        std::path::PathBuf,
        PartitionAssignment,
    ) {
        let config = harness.inner.config.clone();
        let assignment = harness.module.partition_assignment().expect("assigned");
        let path = harness.path.clone();
        drop(harness.inner);
        drop(harness.module);
        (harness._tmp, config, path, assignment)
    }

    fn reopen_module(
        config: &Config,
        path: &std::path::Path,
        assignment: PartitionAssignment,
    ) -> StorageModule {
        StorageModule::new(
            &StorageModuleInfo {
                id: 0,
                partition_assignment: Some(assignment),
                submodules: vec![(partition_chunk_offset_ie!(0, 10), path.to_path_buf())],
            },
            config,
        )
        .expect("reopen storage module")
    }

    /// Open must agree with the in-process settle. The caller has already
    /// dropped the service's decision onto disk.
    fn assert_reopen_chunk(harness: Harness, expected: irys_domain::ChunkType) {
        let (_tmp, config, path, assignment) = close_for_reopen(harness);
        let reopened = reopen_module(&config, &path, assignment);
        assert_eq!(
            reopened.get_chunk_type(&PartitionChunkOffset::from(0)),
            Some(expected),
            "reopen must agree with the in-process settle"
        );
        assert!(!reopened.reset_incomplete());
        assert!(!reset_marker(&reopened));
    }

    /// Failed unassign, no marker. Both applies of capacity→ledger keep entropy.
    /// The flag does not stay set, so the second apply is not a wipe.
    #[tokio::test]
    async fn capacity_to_ledger_after_failed_unassign_keeps_entropy_on_both_applies() {
        use irys_domain::storage_module::ChunkType;

        let mut harness = harness(None);
        write_synced_entropy(&harness.module);
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        let path = harness.path.clone();
        harness.module.fail_next_reset_for_test();
        apply_update(&mut harness, explicit_unassign(&path), 4).await;
        assert!(harness.module.reset_incomplete());
        assert!(!reset_marker(&harness.module));
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            None
        );
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Entropy));

        let update = publish_update(&path, hash, miner);
        apply_update(&mut harness, Arc::clone(&update), 5).await;
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            Some(DataLedger::Publish.into())
        );
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Entropy));
        assert!(
            !harness.module.reset_incomplete(),
            "capacity to ledger must not leave the incomplete flag set"
        );
        assert!(harness.inner.pending_assignment.is_none());
        assert!(!reset_marker(&harness.module));

        apply_update(&mut harness, update, 5).await;
        assert_eq!(
            chunk_type(&harness.module),
            Some(ChunkType::Entropy),
            "the retry must keep the entropy the first apply kept"
        );
        assert!(!harness.module.reset_incomplete());
        assert!(harness.inner.pending_assignment.is_none());
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            Some(DataLedger::Publish.into())
        );
        assert!(harness.receivers.packing.try_recv().is_err());
    }

    /// The marker is durable, so capacity→ledger finishes the clear before
    /// the assign. The retry of that same update does not wipe again.
    #[tokio::test]
    async fn capacity_to_ledger_after_a_durable_marker_finishes_the_wipe() {
        use irys_domain::storage_module::ChunkType;

        let mut harness = harness(None);
        write_synced_entropy(&harness.module);
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        let path = harness.path.clone();
        harness.module.fail_next_reset_for_test();
        apply_update(&mut harness, explicit_unassign(&path), 4).await;
        set_reset_marker(&harness.module);
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Entropy));

        let update = publish_update(&path, hash, miner);
        apply_update(&mut harness, Arc::clone(&update), 5).await;
        assert_eq!(
            chunk_type(&harness.module),
            Some(ChunkType::Uninitialized),
            "a durable marker means the wipe already started"
        );
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            Some(DataLedger::Publish.into())
        );
        assert!(!harness.module.reset_incomplete());
        assert!(!reset_marker(&harness.module));
        assert!(harness.inner.pending_assignment.is_none());
        let request = harness
            .receivers
            .packing
            .try_recv()
            .expect("an assigned ledger packs the wiped span");
        assert_full_partition_span(&request);

        apply_update(&mut harness, update, 5).await;
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Uninitialized));
        assert!(!harness.module.reset_incomplete());
        assert!(harness.inner.pending_assignment.is_none());
        assert!(
            harness.receivers.packing.try_recv().is_err(),
            "the second apply must not pack the span again"
        );
    }

    /// The wipe finished and the ledger write failed. The span is packed
    /// under the assignment that is still stored. The retry stores the
    /// ledger and does not send the span again.
    #[tokio::test]
    async fn capacity_to_ledger_packs_a_finished_wipe_when_the_ledger_write_fails() {
        use irys_domain::storage_module::ChunkType;

        let mut harness = harness(None);
        write_synced_entropy(&harness.module);
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        let path = harness.path.clone();
        harness.module.fail_next_reset_for_test();
        apply_update(&mut harness, explicit_unassign(&path), 4).await;
        set_reset_marker(&harness.module);

        let params_path = path.join(irys_domain::PACKING_PARAMS_FILE_NAME);
        let saved = std::fs::read(&params_path).expect("packing params");
        let _restore = RestoredBytes(params_path.clone(), saved.clone());
        std::fs::write(&params_path, b"not = toml [[[").expect("corrupt packing params");

        let update = publish_update(&path, hash, miner);
        apply_update(&mut harness, Arc::clone(&update), 5).await;
        assert_eq!(
            chunk_type(&harness.module),
            Some(ChunkType::Uninitialized),
            "the marker wipe finishes before the ledger write"
        );
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            None,
            "a failed ledger write leaves the capacity assignment"
        );
        assert!(!harness.module.reset_incomplete());
        assert!(!reset_marker(&harness.module));
        assert!(harness.inner.pending_assignment.is_some());
        let request = harness
            .receivers
            .packing
            .try_recv()
            .expect("the wiped span is packed while the partition stays assigned");
        assert_full_partition_span(&request);

        std::fs::write(&params_path, saved).expect("restore packing params");
        apply_update(&mut harness, update, 5).await;
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            Some(DataLedger::Publish.into())
        );
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Uninitialized));
        assert!(!harness.module.reset_incomplete());
        assert!(harness.inner.pending_assignment.is_none());
        assert!(
            harness.receivers.packing.try_recv().is_err(),
            "the retry must not pack the span again"
        );
    }

    /// The packing-params write fails after the flag is settled. The retry
    /// stores the ledger and still does not wipe.
    #[tokio::test]
    async fn capacity_to_ledger_write_error_after_failed_unassign_does_not_arm_a_wipe() {
        use irys_domain::storage_module::ChunkType;

        let mut harness = harness(None);
        write_synced_entropy(&harness.module);
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        let path = harness.path.clone();
        harness.module.fail_next_reset_for_test();
        apply_update(&mut harness, explicit_unassign(&path), 4).await;
        assert!(harness.module.reset_incomplete());
        assert!(!reset_marker(&harness.module));

        let update = publish_update(&path, hash, miner);
        let _restore = WritableDir(path.clone());
        set_dir_mode(&path, 0o555);
        apply_update(&mut harness, Arc::clone(&update), 5).await;
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Entropy));
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            None
        );
        assert!(
            !harness.module.reset_incomplete(),
            "a failed ledger write must not leave a flag that a later retry wipes"
        );
        assert!(harness.inner.pending_assignment.is_some());

        set_dir_mode(&path, 0o755);
        apply_update(&mut harness, Arc::clone(&update), 5).await;
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Entropy));
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            Some(DataLedger::Publish.into())
        );
        assert!(!harness.module.reset_incomplete());
        assert!(harness.inner.pending_assignment.is_none());

        apply_update(&mut harness, update, 5).await;
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Entropy));
        assert!(harness.receivers.packing.try_recv().is_err());
    }

    /// Open recovery keeps entropy when the reset never wrote its marker.
    #[test]
    fn reopen_keeps_entropy_when_no_reset_marker_was_written() {
        use irys_domain::storage_module::ChunkType;

        let harness = harness(None);
        write_synced_entropy(&harness.module);
        harness.module.fail_next_reset_for_test();
        let err = harness.module.reset().expect_err("injected reset failure");
        assert!(err.to_string().contains("injected"), "{err}");
        assert!(harness.module.reset_incomplete());
        assert!(!reset_marker(&harness.module));
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Entropy));

        let (_tmp, config, path, assignment) = close_for_reopen(harness);
        let reopened = reopen_module(&config, &path, assignment);
        assert_eq!(
            reopened.get_chunk_type(&PartitionChunkOffset::from(0)),
            Some(ChunkType::Entropy),
            "open does not wipe when the marker is absent"
        );
        assert!(!reopened.reset_incomplete());
        assert!(!reset_marker(&reopened));
    }

    /// Open recovery finishes a reset that wrote its marker.
    #[test]
    fn reopen_wipes_entropy_when_the_reset_marker_is_durable() {
        use irys_domain::storage_module::ChunkType;

        let harness = harness(None);
        write_synced_entropy(&harness.module);
        set_reset_marker(&harness.module);
        assert!(reset_marker(&harness.module));
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Entropy));

        let (_tmp, config, path, assignment) = close_for_reopen(harness);
        let reopened = reopen_module(&config, &path, assignment);
        assert_eq!(
            reopened.get_chunk_type(&PartitionChunkOffset::from(0)),
            Some(ChunkType::Uninitialized),
            "open finishes a reset that wrote its marker"
        );
        assert!(!reopened.reset_incomplete());
        assert!(!reset_marker(&reopened));
        assert_eq!(
            reopened.partition_assignment().unwrap().ledger_id,
            None,
            "finishing the marker does not assign the ledger"
        );
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

    /// Writes `bytes` back on drop. Used when a test corrupts a file.
    struct RestoredBytes(std::path::PathBuf, Vec<u8>);

    impl Drop for RestoredBytes {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.0, &self.1);
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
        harness
            .module
            .requeue_packing_range(repair_one_to_three().0);
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
        let packs = harness.module.take_packing_ranges();
        assert!(
            packs.is_empty(),
            "unassigned module must drop a failed pack, still queued: {packs:?}"
        );
        harness.inner.tick();
        let left = harness.module.take_crc_repair_ranges();
        assert!(
            left.is_empty(),
            "a later tick must not restore dropped repairs: {left:?}"
        );
    }

    /// A failed pack is one interval. The tick sends that interval and does
    /// not insert one checksum repair per chunk.
    #[tokio::test]
    async fn tick_sends_a_failed_pack_span_as_one_range() {
        let mut harness = harness(None);
        let span = irys_types::partition_chunk_offset_ii!(0, 9);
        harness.module.requeue_packing_range(span);
        assert_eq!(harness.module.crc_repair_len(), 0);
        harness.inner.tick();
        let request = harness
            .receivers
            .packing
            .try_recv()
            .expect("the tick sends the failed pack as one range");
        assert_eq!(*request.chunk_range().0.start(), 0);
        assert_eq!(*request.chunk_range().0.end(), 9);
        assert!(harness.receivers.packing.try_recv().is_err());
        assert_eq!(harness.module.crc_repair_len(), 0);
        assert!(harness.module.take_packing_ranges().is_empty());
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

    /// A checksum repair is not an uninitialized repack. Entropy in that span
    /// is still sent whole.
    #[tokio::test]
    async fn full_channel_retries_a_crc_repair_that_is_still_entropy() {
        let mut harness = harness(Some(DataLedger::Publish.into()));
        write_span(&harness.module, 1, 3, irys_domain::ChunkType::Entropy);
        fill_packing_channel(&harness);
        harness
            .module
            .requeue_crc_repair_range(repair_one_to_three().0);
        harness.inner.tick();
        assert!(
            harness.receivers.packing.try_recv().is_ok(),
            "one filler leaves so the entropy span can be sent"
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
        assert!(saw, "an entropy checksum span is sent whole");
    }

    /// A capacity-to-ledger update sends the module's deferred spans. A
    /// deferred checksum repair over Entropy is sent whole there, not
    /// narrowed to its `Uninitialized` part and dropped.
    #[tokio::test]
    async fn capacity_to_ledger_sends_a_deferred_crc_repair_whole() {
        let mut harness = harness(None);
        write_span(&harness.module, 1, 3, irys_domain::ChunkType::Entropy);
        fill_packing_channel(&harness);
        harness
            .module
            .requeue_crc_repair_range(repair_one_to_three().0);
        harness.inner.tick();
        let _fillers = drain_packing(&mut harness);
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        let path = harness.path.clone();
        apply_update(&mut harness, publish_update(&path, hash, miner), 5).await;
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            Some(DataLedger::Publish.into())
        );
        let sent = drain_packing(&mut harness);
        assert!(
            sent.iter()
                .any(|request| *request.chunk_range().0.start() == 1
                    && *request.chunk_range().0.end() == 3),
            "the deferred checksum repair is sent whole: {sent:?}"
        );
    }

    fn recovery_span() -> PartitionChunkRange {
        PartitionChunkRange(irys_types::partition_chunk_offset_ii!(1, 3))
    }

    /// A full channel keeps the recovery span. One free slot, then `tick`,
    /// sends that same span.
    #[tokio::test]
    async fn full_channel_retries_a_recovery_repack_once_the_channel_has_room() {
        let mut harness = harness(Some(DataLedger::Publish.into()));
        fill_packing_channel(&harness);
        harness.inner.defer_uninitialized_repack(0, recovery_span());
        let pending = harness
            .inner
            .pending_ranges
            .get(&0)
            .cloned()
            .unwrap_or_default();
        assert_eq!(pending.len(), 1, "a full channel keeps the recovery span");
        assert_eq!(*pending[0].range.0.start(), 1);
        assert_eq!(*pending[0].range.0.end(), 3);

        assert!(
            harness.receivers.packing.try_recv().is_ok(),
            "one filler leaves so the kept span can be sent"
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
        assert!(saw, "the recovery span is sent once the channel has room");
        assert!(!harness.inner.pending_ranges.contains_key(&0));
    }

    /// Data on the deferred span stays. The tick does not pack over it.
    #[tokio::test]
    async fn recovery_repack_stops_once_the_span_is_data() {
        let mut harness = harness(Some(DataLedger::Publish.into()));
        fill_packing_channel(&harness);
        harness.inner.defer_uninitialized_repack(0, recovery_span());
        assert!(
            harness.inner.pending_ranges.contains_key(&0),
            "the span is kept until it is no longer uninitialized"
        );
        write_span(&harness.module, 1, 3, irys_domain::ChunkType::Data);
        let _fillers = drain_packing(&mut harness);
        harness.inner.tick();
        assert!(
            harness.receivers.packing.try_recv().is_err(),
            "a span that is no longer uninitialized is not packed"
        );
        assert!(!harness.inner.pending_ranges.contains_key(&0));
    }

    /// A prefix that became Data stays. Only the uninitialized suffix is sent.
    #[tokio::test]
    async fn recovery_repack_sends_only_the_uninitialized_suffix() {
        let mut harness = harness(Some(DataLedger::Publish.into()));
        fill_packing_channel(&harness);
        harness.inner.defer_uninitialized_repack(0, recovery_span());
        assert!(harness.module.write_chunk(
            PartitionChunkOffset::from(1),
            vec![0xD4_u8; 32],
            irys_domain::ChunkType::Data,
        ));
        harness
            .module
            .force_sync_pending_chunks()
            .expect("sync data prefix");
        assert!(
            harness.receivers.packing.try_recv().is_ok(),
            "one filler leaves so the suffix can be sent"
        );
        harness.inner.tick();
        let mut saw = false;
        for _ in 0..5_001 {
            let Ok(request) = harness.receivers.packing.try_recv() else {
                break;
            };
            if *request.chunk_range().0.start() == 2 && *request.chunk_range().0.end() == 3 {
                saw = true;
                break;
            }
            assert_eq!(
                *request.chunk_range().0.end(),
                0,
                "unexpected packing range {request:?}"
            );
        }
        assert!(saw, "only the uninitialized suffix is sent");
        assert_eq!(
            harness
                .module
                .get_chunk_type(&PartitionChunkOffset::from(1)),
            Some(irys_domain::ChunkType::Data)
        );
    }

    /// A repack message must not clear a stored assignment retry.
    #[tokio::test]
    async fn recovery_repack_does_not_replace_a_pending_assignment() {
        let mut harness = harness(Some(DataLedger::Publish.into()));
        let pending = StorageModuleServiceMessage::PartitionAssignmentsUpdated {
            storage_module_infos: Arc::new(vec![]),
            update_height: 7,
        };
        harness.inner.pending_assignment = Some(pending);
        harness
            .inner
            .on_message(StorageModuleServiceMessage::RepackUninitialized {
                module_id: 0,
                range: recovery_span(),
            })
            .await
            .expect("repack is not an assignment failure");
        match harness.inner.pending_assignment {
            Some(StorageModuleServiceMessage::PartitionAssignmentsUpdated {
                update_height: 7,
                ..
            }) => {}
            other => panic!("repack replaced the stored assignment: {other:?}"),
        }
    }

    /// A full channel keeps the span and says it is waiting. An open channel
    /// sends the same span at once and does not log that wait. After one slot
    /// frees, the kept span is sent once.
    #[tokio::test]
    #[tracing_test::traced_test]
    async fn full_packing_channel_logs_the_kept_span_as_deferred() {
        let mut harness = harness(None);
        harness
            .module
            .requeue_crc_repair_range(repair_one_to_three().0);
        harness.inner.tick();
        let accepted = harness
            .receivers
            .packing
            .try_recv()
            .expect("an open channel accepts the repair");
        assert_eq!(*accepted.chunk_range().0.start(), 1);
        assert_eq!(*accepted.chunk_range().0.end(), 3);
        assert!(harness.receivers.packing.try_recv().is_err());
        assert!(
            !logs_contain("Deferring packing request until the channel accepts it"),
            "an accepted send is not a deferral"
        );
        assert!(
            !logs_contain("Dropping packing request"),
            "an accepted send is not a drop"
        );

        fill_packing_channel(&harness);
        harness
            .module
            .requeue_crc_repair_range(repair_one_to_three().0);
        harness.inner.tick();
        let pending = harness
            .inner
            .pending_ranges
            .get(&0)
            .cloned()
            .unwrap_or_default();
        assert_eq!(pending.len(), 1, "a full channel keeps the repair span");
        assert_eq!(*pending[0].range.0.start(), 1);
        assert_eq!(*pending[0].range.0.end(), 3);
        assert!(
            logs_contain("Deferring packing request until the channel accepts it"),
            "a saturated channel must say the span waits for a free slot"
        );
        assert!(
            !logs_contain("Dropping packing request"),
            "a span that stays queued must not be logged as dropped"
        );

        assert!(
            harness.receivers.packing.try_recv().is_ok(),
            "one filler leaves so the kept span can be sent"
        );
        harness.inner.tick();
        let mut repairs = 0_usize;
        while let Ok(request) = harness.receivers.packing.try_recv() {
            if *request.chunk_range().0.start() == 1 && *request.chunk_range().0.end() == 3 {
                repairs += 1;
            }
        }
        assert_eq!(
            repairs, 1,
            "the kept span is sent once the channel accepts it"
        );
        assert!(!harness.inner.pending_ranges.contains_key(&0));
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
            !harness.inner.pending_ranges.contains_key(&0),
            "an accepted reset pack is not retried"
        );
    }

    /// A one-offset deferred span must not walk every interval fragment.
    /// Pending data inside that offset is not packed. A fresh module still
    /// returns its uninitialized span, and a neighbor offset still packs.
    #[tokio::test]
    async fn deferred_uninitialized_scan_reads_only_the_overlap() -> eyre::Result<()> {
        let mut harness = harness(None);
        let full = PartitionChunkRange(irys_types::partition_chunk_offset_ii!(0, 9));
        harness.inner.pending_ranges.insert(
            0,
            vec![PendingSpan {
                range: full,
                while_uninitialized: true,
            }],
        );
        harness
            .inner
            .enqueue_deferred_uninitialized(&harness.module);
        let fresh = harness
            .receivers
            .packing
            .try_recv()
            .expect("a fresh module packs its uninitialized span");
        assert_eq!(*fresh.chunk_range().0.start(), 0);
        assert_eq!(*fresh.chunk_range().0.end(), 9);
        assert!(harness.receivers.packing.try_recv().is_err());

        for offset in (0..10_u32).filter(|offset| offset % 2 == 0) {
            assert!(
                harness.module.write_chunk(
                    PartitionChunkOffset::from(offset),
                    vec![0xE1_u8; 32],
                    irys_domain::ChunkType::Entropy,
                ),
                "entropy at {offset}"
            );
        }
        harness.module.force_sync_pending_chunks()?;
        let entropy = harness
            .module
            .get_intervals(irys_domain::ChunkType::Entropy)
            .len();
        assert!(
            entropy >= 5,
            "even offsets did not fragment the interval map: {entropy}"
        );

        let one = PartitionChunkRange(irys_types::partition_chunk_offset_ii!(1, 1));
        harness.inner.pending_ranges.insert(
            0,
            vec![PendingSpan {
                range: one,
                while_uninitialized: true,
            }],
        );
        let before = harness.module.interval_visit_count();
        harness
            .inner
            .enqueue_deferred_uninitialized(&harness.module);
        let visits = harness.module.interval_visit_count() - before;
        let packed = harness
            .receivers
            .packing
            .try_recv()
            .expect("offset 1 stays uninitialized");
        assert_eq!(*packed.chunk_range().0.start(), 1);
        assert_eq!(*packed.chunk_range().0.end(), 1);
        assert!(visits <= 2, "one-offset query visited {visits} intervals");
        assert!(harness.receivers.packing.try_recv().is_err());

        assert!(harness.module.write_chunk(
            PartitionChunkOffset::from(3),
            vec![0xD4_u8; 32],
            irys_domain::ChunkType::Data,
        ));
        let pending = PartitionChunkRange(irys_types::partition_chunk_offset_ii!(3, 3));
        harness.inner.pending_ranges.insert(
            0,
            vec![PendingSpan {
                range: pending,
                while_uninitialized: true,
            }],
        );
        harness
            .inner
            .enqueue_deferred_uninitialized(&harness.module);
        assert!(
            harness.receivers.packing.try_recv().is_err(),
            "pending data inside the range is not packed"
        );

        let neighbor = PartitionChunkRange(irys_types::partition_chunk_offset_ii!(5, 5));
        harness.inner.pending_ranges.insert(
            0,
            vec![PendingSpan {
                range: neighbor,
                while_uninitialized: true,
            }],
        );
        harness
            .inner
            .enqueue_deferred_uninitialized(&harness.module);
        let neighbor_req = harness
            .receivers
            .packing
            .try_recv()
            .expect("offset 5 stays uninitialized");
        assert_eq!(*neighbor_req.chunk_range().0.start(), 5);
        assert_eq!(*neighbor_req.chunk_range().0.end(), 5);
        Ok(())
    }

    fn write_span(module: &StorageModule, from: u32, to: u32, kind: irys_domain::ChunkType) {
        for offset in from..=to {
            assert!(
                module.write_chunk(PartitionChunkOffset::from(offset), vec![0x5A_u8; 32], kind),
                "write offset {offset}"
            );
        }
        module.force_sync_pending_chunks().expect("sync span");
    }

    /// Ledger→capacity while the channel is full, then capacity→ledger.
    /// The reset span is still `Uninitialized` because the channel never
    /// accepted it. The ledger slot must be packed.
    async fn defer_capacity_reset(harness: &mut Harness) {
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        let path = harness.path.clone();
        fill_packing_channel(harness);
        apply_update(harness, capacity_update(&path, hash, miner), 4).await;
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            None
        );
        assert!(
            harness.inner.pending_ranges.contains_key(&0),
            "a full channel keeps the reset span"
        );
        let uninitialized = harness
            .module
            .get_intervals(irys_domain::ChunkType::Uninitialized);
        assert_eq!(
            uninitialized.len(),
            1,
            "reset publishes one uninitialized span"
        );
        assert_eq!(*uninitialized[0].start(), 0);
        assert_eq!(*uninitialized[0].end(), 9);
    }

    /// The channel is still full at the epoch boundary. After it drains, tick
    /// sends the uninitialized span the capacity reset could not send.
    #[tokio::test]
    async fn capacity_to_ledger_sends_a_deferred_uninitialized_span() {
        let mut harness = harness(Some(DataLedger::Publish.into()));
        defer_capacity_reset(&mut harness).await;
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        let path = harness.path.clone();
        apply_update(&mut harness, publish_update(&path, hash, miner), 5).await;
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            Some(DataLedger::Publish.into())
        );
        assert_eq!(
            harness
                .module
                .get_chunk_type(&PartitionChunkOffset::from(0)),
            Some(irys_domain::ChunkType::Uninitialized)
        );
        let _fillers = drain_packing(&mut harness);
        harness.inner.tick();
        let request = harness
            .receivers
            .packing
            .try_recv()
            .expect("capacity to ledger must pack a reset span the channel never accepted");
        assert_full_partition_span(&request);
        assert!(!harness.inner.pending_ranges.contains_key(&0));
        assert!(harness.receivers.packing.try_recv().is_err());
    }

    /// The channel has room before the epoch boundary. The update itself sends
    /// the uninitialized span. It does not wait for a later tick.
    #[tokio::test]
    async fn capacity_to_ledger_sends_a_deferred_span_when_the_channel_has_room() {
        let mut harness = harness(Some(DataLedger::Publish.into()));
        defer_capacity_reset(&mut harness).await;
        let _fillers = drain_packing(&mut harness);
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        let path = harness.path.clone();
        apply_update(&mut harness, publish_update(&path, hash, miner), 5).await;
        let request = harness
            .receivers
            .packing
            .try_recv()
            .expect("an open channel accepts the uninitialized span during capacity to ledger");
        assert_full_partition_span(&request);
        assert!(!harness.inner.pending_ranges.contains_key(&0));
    }

    /// The capacity pack already landed. Capacity→ledger must not send the
    /// remembered span once those offsets are Entropy.
    #[tokio::test]
    async fn capacity_to_ledger_drops_a_deferred_span_that_is_entropy() {
        let mut harness = harness(Some(DataLedger::Publish.into()));
        defer_capacity_reset(&mut harness).await;
        write_span(&harness.module, 0, 9, irys_domain::ChunkType::Entropy);
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        let path = harness.path.clone();
        apply_update(&mut harness, publish_update(&path, hash, miner), 5).await;
        assert_eq!(
            harness
                .module
                .get_chunk_type(&PartitionChunkOffset::from(3)),
            Some(irys_domain::ChunkType::Entropy)
        );
        assert!(
            !harness.inner.pending_ranges.contains_key(&0),
            "entropy offsets are not packed again"
        );
        let _fillers = drain_packing(&mut harness);
        harness.inner.tick();
        assert!(
            harness.receivers.packing.try_recv().is_err(),
            "capacity to ledger must not repack entropy"
        );
    }

    /// Data on the deferred span is the ledger's bytes. Do not pack over it.
    #[tokio::test]
    async fn capacity_to_ledger_drops_a_deferred_span_that_is_data() {
        let mut harness = harness(Some(DataLedger::Publish.into()));
        defer_capacity_reset(&mut harness).await;
        write_span(&harness.module, 0, 9, irys_domain::ChunkType::Data);
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        let path = harness.path.clone();
        apply_update(&mut harness, publish_update(&path, hash, miner), 5).await;
        assert_eq!(
            harness
                .module
                .get_chunk_type(&PartitionChunkOffset::from(0)),
            Some(irys_domain::ChunkType::Data)
        );
        assert!(!harness.inner.pending_ranges.contains_key(&0));
        let _fillers = drain_packing(&mut harness);
        harness.inner.tick();
        assert!(
            harness.receivers.packing.try_recv().is_err(),
            "capacity to ledger must not pack over data"
        );
    }

    /// A prefix that became Data stays. Only the uninitialized suffix is sent.
    #[tokio::test]
    async fn capacity_to_ledger_sends_only_the_uninitialized_suffix() {
        let mut harness = harness(Some(DataLedger::Publish.into()));
        defer_capacity_reset(&mut harness).await;
        write_span(&harness.module, 0, 4, irys_domain::ChunkType::Data);
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        let path = harness.path.clone();
        apply_update(&mut harness, publish_update(&path, hash, miner), 5).await;
        let _fillers = drain_packing(&mut harness);
        harness.inner.tick();
        let request = harness
            .receivers
            .packing
            .try_recv()
            .expect("the uninitialized suffix is still packed");
        assert_eq!(*request.chunk_range().0.start(), 5);
        assert_eq!(*request.chunk_range().0.end(), 9);
        assert!(harness.receivers.packing.try_recv().is_err());
        assert_eq!(
            harness
                .module
                .get_chunk_type(&PartitionChunkOffset::from(0)),
            Some(irys_domain::ChunkType::Data)
        );
    }

    /// A packing-params write error must not drop the uninitialized span.
    /// The retry still sends it.
    #[tokio::test]
    async fn capacity_to_ledger_write_error_keeps_the_deferred_span() {
        let mut harness = harness(Some(DataLedger::Publish.into()));
        defer_capacity_reset(&mut harness).await;
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        let path = harness.path.clone();
        let update = publish_update(&path, hash, miner);
        let _restore = WritableDir(path.clone());
        set_dir_mode(&path, 0o555);
        harness
            .inner
            .apply_assignment(StorageModuleServiceMessage::PartitionAssignmentsUpdated {
                storage_module_infos: Arc::clone(&update),
                update_height: 5,
            })
            .await
            .expect("a packing-params write error is retried");
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            None,
            "a failed write leaves the capacity assignment"
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
            "a failed capacity-to-ledger write must keep the uninitialized span"
        );
        assert_eq!(*pending[0].range.0.start(), 0);
        assert_eq!(*pending[0].range.0.end(), 9);

        set_dir_mode(&path, 0o755);
        harness
            .inner
            .apply_assignment(StorageModuleServiceMessage::PartitionAssignmentsUpdated {
                storage_module_infos: update,
                update_height: 5,
            })
            .await
            .expect("retry stores the ledger");
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            Some(DataLedger::Publish.into())
        );
        let _fillers = drain_packing(&mut harness);
        harness.inner.tick();
        let request = harness
            .receivers
            .packing
            .try_recv()
            .expect("the retry packs the span the failed write kept");
        assert_full_partition_span(&request);
    }

    fn assert_one_deferred_full_span(harness: &Harness) {
        let pending = harness
            .inner
            .pending_ranges
            .get(&0)
            .cloned()
            .unwrap_or_default();
        assert_eq!(pending.len(), 1, "a full channel keeps one wipe span");
        assert_eq!(*pending[0].range.0.start(), 0);
        assert_eq!(*pending[0].range.0.end(), 9);
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
        assert!(!harness.inner.pending_ranges.contains_key(&0));
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

    /// A marker wipe on capacity→ledger publishes `Uninitialized` while the
    /// channel is full. The span stays one range and is sent once it has room.
    #[tokio::test]
    async fn full_channel_defers_a_capacity_to_ledger_marker_wipe() {
        use irys_domain::storage_module::ChunkType;

        let mut harness = harness(None);
        write_synced_entropy(&harness.module);
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        let path = harness.path.clone();
        harness.module.fail_next_reset_for_test();
        apply_update(&mut harness, explicit_unassign(&path), 4).await;
        set_reset_marker(&harness.module);
        fill_packing_channel(&harness);
        apply_update(&mut harness, publish_update(&path, hash, miner), 5).await;
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Uninitialized));
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            Some(DataLedger::Publish.into())
        );
        assert_one_deferred_full_span(&harness);
        deferred_span_is_sent_on_tick(&mut harness);
        assert!(harness.receivers.packing.try_recv().is_err());
    }

    /// A marker wipe on a matching ledger update publishes `Uninitialized`
    /// while the channel is full. One span is kept and sent later.
    #[tokio::test]
    async fn full_channel_defers_a_matching_ledger_wipe() {
        use irys_domain::storage_module::ChunkType;

        let mut harness = harness(Some(DataLedger::Publish.into()));
        write_synced_data(&harness.module, 0x41);
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        let path = harness.path.clone();
        harness.module.fail_next_reset_for_test();
        apply_update(&mut harness, capacity_update(&path, hash, miner), 4).await;
        set_reset_marker(&harness.module);
        fill_packing_channel(&harness);
        apply_update(&mut harness, publish_update(&path, hash, miner), 5).await;
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Uninitialized));
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            Some(DataLedger::Publish.into())
        );
        assert_one_deferred_full_span(&harness);
        deferred_span_is_sent_on_tick(&mut harness);
        assert!(harness.receivers.packing.try_recv().is_err());
    }

    /// A marker wipe on a stale unassign publishes `Uninitialized` while the
    /// channel is full. The newer ledger stays, and one span is sent later.
    #[tokio::test]
    async fn full_channel_defers_a_newer_local_ledger_wipe() {
        use irys_domain::storage_module::ChunkType;

        let (mut harness, hash, miner) = store_slot_one_with_data(50, 0x42).await;
        let path = harness.path.clone();
        harness.module.fail_next_reset_for_test();
        apply_update(&mut harness, capacity_update(&path, hash, miner), 60).await;
        set_reset_marker(&harness.module);
        fill_packing_channel(&harness);
        apply_update(&mut harness, explicit_unassign(&path), 10).await;
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Uninitialized));
        assert_eq!(
            harness.module.partition_assignment().unwrap().slot_index,
            Some(1)
        );
        assert_one_deferred_full_span(&harness);
        deferred_span_is_sent_on_tick(&mut harness);
        assert!(harness.receivers.packing.try_recv().is_err());
    }

    /// A dropped retry finishes a marker wipe while the channel is full.
    /// The tick keeps one span and sends it once the channel has room.
    #[tokio::test]
    async fn full_channel_defers_a_dropped_ledger_wipe() {
        use irys_domain::storage_module::ChunkType;

        let mut harness = harness(Some(DataLedger::Publish.into()));
        write_synced_data(&harness.module, 0x43);
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        let path = harness.path.clone();
        harness.module.fail_next_reset_for_test();
        apply_update(&mut harness, capacity_update(&path, hash, miner), 4).await;
        set_reset_marker(&harness.module);
        harness.inner.pending_assignment = None;
        fill_packing_channel(&harness);
        harness.inner.tick();
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Uninitialized));
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            Some(DataLedger::Publish.into())
        );
        assert_one_deferred_full_span(&harness);
        deferred_span_is_sent_on_tick(&mut harness);
        assert!(harness.receivers.packing.try_recv().is_err());
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
        assert!(harness.inner.pending_ranges.contains_key(&0));
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
            !harness.inner.pending_ranges.contains_key(&0),
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
        assert_eq!(*pending[0].range.0.start(), 0);
        assert_eq!(*pending[0].range.0.end(), 9);
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

    /// Drive `start` until two periodic heals have begun. Returns how many
    /// 1s-arm runs happened after the first pass returned.
    async fn fast_ticks_between_heal_passes(hold: Duration) -> u64 {
        let probe = Arc::new(HealScheduleProbe {
            hold,
            passes: AtomicU64::new(0),
            ticks_since_pass: AtomicU64::new(0),
            ticks_between_passes: AtomicU64::new(0),
        });
        let mut harness = harness(None);
        // No module, so the loop does not start a disk-lane thread.
        harness.inner.storage_modules.write().unwrap().clear();
        harness.inner.heal_probe = Some(Arc::clone(&probe));
        let Harness {
            _tmp,
            _shutdown_tx,
            receivers: _receivers,
            inner,
            ..
        } = harness;
        let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();
        let (msg_tx, msg_rx) = tokio::sync::mpsc::unbounded_channel();
        let service = StorageModuleService {
            shutdown: shutdown_rx,
            msg_rx,
            inner,
        };
        let handle = tokio::spawn(async move {
            service.start().await.expect("storage module service start");
        });
        for _ in 0..500 {
            if probe.passes.load(Ordering::Relaxed) >= 2 || handle.is_finished() {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let passes = probe.passes.load(Ordering::Relaxed);
        let between = probe.ticks_between_passes.load(Ordering::Relaxed);
        shutdown_tx.fire();
        drop(msg_tx);
        let joined = handle.await;
        assert!(
            passes >= 2,
            "periodic heal passes: {passes}, join: {joined:?}"
        );
        drop(_tmp);
        drop(_shutdown_tx);
        drop(_receivers);
        between
    }

    /// A pass that lasts the whole retry period must not chain into the next
    /// one. The 1s arm is the only place CRC repair and a stored assignment
    /// retry run.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn budget_length_heal_runs_the_one_second_arm_between_passes() {
        let between =
            fast_ticks_between_heal_passes(super::index_heal::INDEX_HEAL_RETRY_INTERVAL).await;
        assert!(
            between > 0,
            "1s arm runs between two budget-length heals, got {between}"
        );
    }

    /// A pass that finishes inside the period keeps the original deadline.
    /// Pushing every pass out by another full period would about double the
    /// 1s ticks between passes.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn short_heal_pass_keeps_the_retry_interval() {
        let hold =
            super::index_heal::INDEX_HEAL_RETRY_INTERVAL.saturating_sub(Duration::from_secs(1));
        let between = fast_ticks_between_heal_passes(hold).await;
        assert!(
            between > 0 && between < 90,
            "a pass one second under the period still retries on that period, got {between} 1s ticks"
        );
    }
}
