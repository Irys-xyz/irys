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
use index_heal::{HealOutcome, INDEX_HEAL_RETRY_INTERVAL, IndexHealCtx, heal_ledger_data_indexes};
use irys_config::StorageSubmodulesConfig;
use irys_domain::{
    BlockIndexReadGuard, BlockTreeReadGuard, DriveFault, EpochSnapshot, Held, StorageModule,
    drive_verdict::{
        self, ChainIdentity, DriveAction, DriveDecision, DriveFile, DriveVerdict, Verdict,
        VerdictInputs,
    },
    stop_node_for_drive_fault,
};
use irys_types::{
    Config, H256, IrysAddress, PartitionChunkRange, TokioServiceHandle, Traced,
    partition::PartitionAssignment,
};
use reth::tasks::shutdown::Shutdown;
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::{Arc, RwLock},
    thread::JoinHandle,
    time::Duration,
};
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{Instrument as _, error, info, warn};

// Messages that the StorageModuleService service supports
#[derive(Debug, Clone)]
pub enum StorageModuleServiceMessage {
    /// The canonical epoch snapshot changed (an epoch block, a reorg that
    /// orphaned the only epoch block, or startup). The service runs
    /// [`drive_verdict::decide`] over what its open modules record in memory
    /// and applies the result. Build it with
    /// [`StorageModuleServiceMessage::assignments_updated`].
    PartitionAssignmentsUpdated {
        /// This miner's assignments, from the canonical epoch snapshot.
        assignments: Arc<Vec<PartitionAssignment>>,
        /// The snapshot's epoch height. Verdicts are judged on it.
        epoch_height: u64,
        /// The node was in sync with the network head when the epoch was
        /// processed. Only then is the packing file of a drive that is ours
        /// stamped as v2 (a v1 file, another
        /// genesis, or no trusted assigned height).
        in_sync: bool,
    },
    /// Rollback marked this span `Uninitialized` and the packing channel did
    /// not accept it. `tick` resends it while any offset is still
    /// `Uninitialized`.
    RepackUninitialized {
        module_id: usize,
        range: PartitionChunkRange,
    },
}

impl StorageModuleServiceMessage {
    /// The assignments update for `snapshot`: `miner`'s assignments at the
    /// snapshot's epoch height.
    pub fn assignments_updated(
        snapshot: &EpochSnapshot,
        miner: IrysAddress,
        in_sync: bool,
    ) -> Self {
        Self::PartitionAssignmentsUpdated {
            assignments: Arc::new(snapshot.get_partition_assignments(miner)),
            epoch_height: snapshot.epoch_height,
            in_sync,
        }
    }
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
    /// This node's chain identity, from the genesis item startup read.
    identity: ChainIdentity,
    /// Verdict and action last logged per module id. INFO is logged only for
    /// a change.
    last_decisions: HashMap<usize, (DriveVerdict, DriveAction)>,
    /// Assignments no drive held or received at the last update.
    last_unplaced: Vec<H256>,
    /// The non-mining warning was logged (see
    /// [`Self::warn_if_assigned_without_mining`]).
    non_mining_warned: bool,
    /// Spans the packing channel did not accept. `tick` sends them until the
    /// channel accepts. A range stays one interval, so a full partition is not
    /// expanded into one repair offset per chunk.
    pending_ranges: HashMap<usize, Vec<PendingSpan>>,
    /// Test stand-in for a periodic heal pass. `None` in every other test.
    #[cfg(test)]
    heal_probe: Option<Arc<HealScheduleProbe>>,
    /// Finalized-resize worker. Yields the activation hash once every module
    /// completed, `None` when any module must be retried on a later tick.
    resize_task: Option<tokio::task::JoinHandle<Option<H256>>>,
    /// Activation whose finalized resize every module completed; stops the
    /// per-tick worker spawn.
    resize_done: Option<H256>,
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
        identity: ChainIdentity,
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
            resize_task: None,
            resize_done: None,
            index_heal_needs_retry: true, // force heal until first pass reports clean
            identity,
            last_decisions: HashMap::new(),
            last_unplaced: Vec::new(),
            non_mining_warned: false,
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

    /// Assignment updates go through [`Self::apply_assignment`], which stops
    /// the node on a drive fault. A repack only records its span.
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
                assignments,
                epoch_height,
                in_sync,
            } => {
                self.handle_partition_assignments_update(assignments, epoch_height, in_sync)
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

    /// Apply one assignment update. A drive fault stops the node here, the
    /// one place it is handled. Any other error aborts the service.
    async fn apply_assignment(&mut self, msg: StorageModuleServiceMessage) -> eyre::Result<()> {
        let result = self.handle_message(msg).await;
        if let Err(err) = &result
            && let Some(fault) = err.downcast_ref::<DriveFault>()
        {
            stop_node_for_drive_fault(&format!("{fault}; stopping the node"));
        }
        result
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

    fn start_partition_resize(&mut self) -> eyre::Result<()> {
        if self.resize_task.is_some() {
            return Ok(());
        }
        let epoch = self.block_tree.read().canonical_epoch_snapshot();
        let Some(resize) = epoch.partition_resize else {
            return Ok(());
        };
        if self.resize_done == Some(resize.activation_hash) {
            return Ok(());
        }
        let index = self.block_index.read();
        let finalized = irys_domain::forkchoice_markers::finalized_height(
            index.latest_height(),
            self.config.consensus.block_tree_depth,
            u64::from(self.config.consensus.block_migration_depth),
        );
        if finalized < resize.activation_height {
            return Ok(());
        }
        eyre::ensure!(
            index
                .get_item(resize.activation_height)
                .is_some_and(|b| b.block_hash == resize.activation_hash),
            "finalized partition resize activation differs from canonical branch"
        );
        // Finality is irreversible. The block index is a shared read handle;
        // clone modules out of the registry lock before any disk IO and run
        // migration on the blocking pool rather than on this service's tick.
        let modules = self.storage_modules.read().unwrap().clone();
        self.resize_task = Some(tokio::task::spawn_blocking(move || {
            let mut all_done = true;
            for module in modules {
                match module.finalize_partition_resize(&resize) {
                    Ok(true) => tracing::info!(
                        storage_module.id = module.id,
                        activation.height = resize.activation_height,
                        retained_chunks = resize.params.partition_chunks,
                        "Completed finalized partition resize"
                    ),
                    Ok(false) => {}
                    Err(e) => {
                        all_done = false;
                        error!(
                            storage_module.id = module.id,
                            "partition resize failed; will retry: {e:?}"
                        );
                    }
                }
            }
            all_done.then_some(resize.activation_hash)
        }));
        Ok(())
    }

    #[tracing::instrument(level = "trace", skip_all)]
    async fn tick(&mut self) {
        if self
            .resize_task
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
        {
            match self.resize_task.take().expect("finished task").await {
                Ok(Some(activation_hash)) => self.resize_done = Some(activation_hash),
                Ok(None) => {}
                Err(e) => error!("partition resize worker failed; will retry: {e:?}"),
            }
        }
        if let Err(e) = self.start_partition_resize() {
            error!("finalized partition resize failed; will retry: {e:?}");
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
            self.enqueue_regrown_suffix(sm);
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

    /// One verdict input per open module, in id order, from the module's
    /// in-memory record of its packing file. Files are never read here: every
    /// write path updates that record. Every configured drive is open, so the
    /// ids are the configured path indexes. Returns the inputs and the module
    /// id of each entry.
    fn runtime_drives(
        &self,
        modules: &BTreeMap<usize, Arc<StorageModule>>,
    ) -> (Vec<(PathBuf, DriveFile)>, Vec<usize>) {
        let ids: Vec<usize> = modules.keys().copied().collect();
        let drives = ids
            .iter()
            .map(|id| {
                let path = self
                    .submodules_config
                    .submodule_paths
                    .get(*id)
                    .cloned()
                    .unwrap_or_default();
                (path, DriveFile::Recorded(modules[id].recorded_params()))
            })
            .collect();
        (drives, ids)
    }

    /// Runs the drive verdict over the open modules and applies it. Only what
    /// differs from a module's in-memory state is changed.
    #[tracing::instrument(level = "trace", skip_all, err, fields(epoch.height = epoch_height))]
    async fn handle_partition_assignments_update(
        &mut self,
        assignments: Arc<Vec<PartitionAssignment>>,
        epoch_height: u64,
        in_sync: bool,
    ) -> eyre::Result<()> {
        let modules: BTreeMap<usize, Arc<StorageModule>> = {
            let modules_guard = self.storage_modules.read().unwrap();
            modules_guard
                .iter()
                .map(|module| (module.id, module.clone()))
                .collect()
        };
        let identity = self.identity;
        let (drives, ids) = self.runtime_drives(&modules);
        let inputs = VerdictInputs {
            drives: &drives,
            assignments: &assignments,
            epoch_height,
            in_sync,
            identity: &identity,
        };
        let verdict = drive_verdict::decide(&inputs);
        self.log_changed_decisions(&ids, &verdict, &inputs);

        for decision in &verdict.decisions {
            let module = &modules[&ids[decision.id]];
            self.apply_decision(module, decision, epoch_height).await?;
        }
        self.warn_if_assigned_without_mining(&modules);

        // After assignments settle: best-effort gap-scan + index backfill for every
        // local SM on a data ledger. Do not fail membership updates if heal hard-
        // errors (channel closed); periodic retry picks up via needs_retry.
        if let Err(e) = self.run_index_heal().await {
            error!("index heal after partition assignment failed: {e:?}");
            self.index_heal_needs_retry = true;
        }

        Ok(())
    }

    /// INFO for each drive whose verdict or action changed since the last
    /// update, WARN when the set of assignments no drive holds changed.
    fn log_changed_decisions(
        &mut self,
        ids: &[usize],
        verdict: &Verdict,
        inputs: &VerdictInputs<'_>,
    ) {
        for decision in &verdict.decisions {
            let id = ids[decision.id];
            let now = (decision.verdict.clone(), decision.action.clone());
            if self.last_decisions.get(&id) == Some(&now) {
                continue;
            }
            let target = match &decision.action {
                DriveAction::Assign(assignment) => Some(*assignment),
                DriveAction::Idle | DriveAction::Clear => None,
            };
            info!(
                storage_module.id = id,
                storage_module.path = %decision.path.display(),
                drive.verdict = decision.verdict.name(),
                drive.reason = %decision.verdict.reason(),
                drive.action = decision.action.name(),
                partition.hash = ?target.map(|pa| pa.partition_hash),
                partition.ledger_id = ?target.and_then(|pa| pa.ledger_id),
                partition.slot_index = ?target.and_then(|pa| pa.slot_index),
                epoch.height = inputs.epoch_height,
                "drive verdict changed"
            );
            self.last_decisions.insert(id, now);
        }
        let hashes: Vec<H256> = verdict.unplaced.iter().map(|a| a.partition_hash).collect();
        if hashes != self.last_unplaced {
            for assignment in &verdict.unplaced {
                warn!(
                    partition.hash = %assignment.partition_hash,
                    partition.ledger_id = ?assignment.ledger_id,
                    partition.slot_index = ?assignment.slot_index,
                    "no configured drive holds or receives this assigned partition"
                );
            }
            self.last_unplaced = hashes;
        }
    }

    /// A node converted from a mining mode keeps the partitions its drives
    /// hold. It will not mine them, and the network cannot reassign a slot
    /// that is still pledged, so the capacity is stranded. Say so once, after
    /// the first update that leaves a module assigned, rather than leaving
    /// the operator to infer it from packing on a node expected to go quiet.
    fn warn_if_assigned_without_mining(&mut self, modules: &BTreeMap<usize, Arc<StorageModule>>) {
        let node_mode = self.config.node_config.node_mode;
        if self.non_mining_warned || node_mode.mines() {
            return;
        }
        let assigned = modules
            .values()
            .filter(|module| module.partition_assignment().is_some())
            .count();
        if assigned > 0 {
            self.non_mining_warned = true;
            warn!(
                "node_mode {node_mode:?} does not mine, but {assigned} storage module(s) \
                 still hold partition assignments. This node keeps packing them and keeps \
                 their slots pledged without ever mining them. Submit unpledge commitments \
                 for those partitions if you meant to release the capacity."
            );
        }
    }

    /// Apply one drive's decision to its open module. The module picks the
    /// transition ([`StorageModule::hold`]); the service packs what it says.
    /// A drive fault is returned as is: the message boundary stops the node
    /// on it, and there is no retry. The transition (a wipe or slot exit
    /// writes the whole drive) runs on the blocking pool.
    async fn apply_decision(
        &mut self,
        module: &Arc<StorageModule>,
        decision: &DriveDecision,
        epoch_height: u64,
    ) -> Result<(), DriveFault> {
        let next = match &decision.action {
            DriveAction::Idle => {
                self.idle_module(module);
                return Ok(());
            }
            DriveAction::Clear => None,
            DriveAction::Assign(assignment) => Some(*assignment),
        };
        let stamp_v2 = decision.stamp_v2;
        let drive = Arc::clone(module);
        let held =
            match tokio::task::spawn_blocking(move || drive.hold(next, epoch_height, stamp_v2))
                .await
            {
                Ok(held) => held?,
                Err(join) => std::panic::resume_unwind(join.into_panic()),
            };
        match held {
            Held::Kept => {}
            Held::Adopted => self.enqueue_uninitialized(module),
            // Entropy and Data stay. Send only a span the channel has not
            // accepted that is still Uninitialized.
            Held::GainedLedger => self.enqueue_deferred_uninitialized(module),
            Held::LeftSlot(spans) => {
                info!(
                    storage_module.id = module.id,
                    packing.spans = spans.len(),
                    "storage module left its ledger slot; entropy kept, old slot data uninitialized"
                );
                // Spans the channel did not accept belong to the old slot.
                // The exit turned what still needs packing into Uninitialized.
                self.pending_ranges.remove(&module.id);
                self.enqueue_uninitialized(module);
            }
            Held::Reset(range) => self.enqueue_packing(module, PartitionChunkRange(range)),
            Held::Released(range) => {
                // The module is unassigned. Do not pack a span the channel deferred.
                self.pending_ranges.remove(&module.id);
                info!(
                    storage_module.id = module.id,
                    packing.interval = ?range,
                    "released storage module wiped; it records no partition now"
                );
            }
        }
        Ok(())
    }

    /// Idle: no assignment in memory, file and bytes untouched.
    fn idle_module(&mut self, module: &Arc<StorageModule>) {
        if let Some(held) = module.park_assignment() {
            warn!(
                storage_module.id = module.id,
                partition.hash = %held.partition_hash,
                partition.ledger_id = ?held.ledger_id,
                partition.slot_index = ?held.slot_index,
                "storage module parked: the canonical epoch does not let it keep this assignment yet; its file and bytes are kept"
            );
        }
        // The module cannot pack. A span kept for a full channel is dropped.
        self.pending_ranges.remove(&module.id);
    }

    /// Send every Uninitialized span of `module`. A full channel keeps them.
    fn enqueue_uninitialized(&mut self, module: &Arc<StorageModule>) {
        let mut deferred = Vec::new();
        for interval in module.get_intervals(irys_domain::ChunkType::Uninitialized) {
            self.enqueue_uninitialized_span(module, PartitionChunkRange(interval), &mut deferred);
        }
        if !deferred.is_empty() {
            self.pending_ranges
                .entry(module.id)
                .or_default()
                .extend(deferred);
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

    /// Pack the suffix a growing partition bound re-exposed. Only its
    /// `Uninitialized` spans are sent; a span the channel does not accept is
    /// kept with the other deferred ranges and sent by a later tick.
    fn enqueue_regrown_suffix(&mut self, module: &Arc<StorageModule>) {
        let Some(range) = module.take_regrown_suffix() else {
            return;
        };
        if module.partition_assignment().is_none() {
            // Assignment packs the module up to the bound then in force.
            return;
        }
        let mut deferred = Vec::new();
        for overlap in module.uninitialized_spans_overlapping(range) {
            let span = PartitionChunkRange(overlap);
            match self.try_enqueue_packing(module, span) {
                PackSend::Accepted | PackSend::Rejected => {}
                PackSend::Deferred => deferred.push(PendingSpan {
                    range: span,
                    while_uninitialized: true,
                }),
            }
        }
        if !deferred.is_empty() {
            self.pending_ranges
                .entry(module.id)
                .or_default()
                .extend(deferred);
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
        identity: ChainIdentity,
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
                        identity,
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
                    // pass would start first. CRC repair runs only on that 1s
                    // arm. Wait one period from now.
                    if started.elapsed() >= INDEX_HEAL_RETRY_INTERVAL {
                        heal_interval.reset();
                    }
                }
                _ = interval.tick() => {
                    #[cfg(test)]
                    if let Some(probe) = &self.inner.heal_probe {
                        probe.record_fast_tick();
                    }
                    self.inner.tick().await;
                }
            }
        }

        tracing::debug!(custom.amount_of_messages = ?self.msg_rx.len(), "processing last in-bound messages before shutdown");
        while let Ok(traced) = self.msg_rx.try_recv() {
            let (msg, parent_span) = traced.into_parts();
            let span = tracing::trace_span!(parent: &parent_span, "storage_module_handle_message");
            self.inner.on_message(msg).instrument(span).await?
        }

        if let Some(task) = self.inner.resize_task.take()
            && let Err(e) = task.await
        {
            error!("partition resize worker failed during shutdown: {e:?}");
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
        BlockIndex, BlockTree, BlockTreeReadGuard, PackingParams, StorageModule, StorageModuleInfo,
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
    }

    fn harness(ledger_id: Option<u32>) -> Harness {
        open_harness(ledger_id, |_| {})
    }

    /// Genesis block hash the harness service and module agree on.
    const GENESIS: H256 = H256::repeat_byte(0x9E);

    /// A module opened as startup opens it (chain identity set), with its
    /// file rewritten as v2 with assigned height 0, so an unassign is a
    /// release.
    fn open_harness(ledger_id: Option<u32>, configure: impl FnOnce(&mut NodeConfig)) -> Harness {
        open_harness_with(ledger_id, configure, true)
    }

    /// `stamp_v2 = false` keeps the module's file as open wrote it (v1).
    fn open_harness_with(
        ledger_id: Option<u32>,
        configure: impl FnOnce(&mut NodeConfig),
        stamp_v2: bool,
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
        let submodules = StorageSubmodulesConfig::load_for_test(tmp.path().to_path_buf(), 1)
            .expect("submodules");
        let config = Config::new_with_random_peer_id(node_config);
        let assignment = PartitionAssignment {
            partition_hash: H256::repeat_byte(0x21),
            miner_address: config.node_config.miner_address(),
            ledger_id,
            slot_index: ledger_id.map(|_| 0),
        };
        let path = submodules.submodule_paths[0].clone();
        let module = open_module(&config, &path, Some(assignment));
        if stamp_v2 {
            module.assign_partition(assignment, 0).expect("stamp v2");
        }
        service_harness(tmp, config, module, path)
    }

    /// Open the module at `path` and set its chain identity.
    fn open_module(
        config: &Config,
        path: &std::path::Path,
        partition_assignment: Option<PartitionAssignment>,
    ) -> Arc<StorageModule> {
        let info = StorageModuleInfo {
            id: 0,
            partition_assignment,
            submodules: vec![(partition_chunk_offset_ie!(0, 10), path.to_path_buf())],
        };
        let module = StorageModule::new(&info, config).expect("storage module");
        module.set_chain_identity(GENESIS);
        Arc::new(module)
    }

    /// A service over `module` alone, with an empty block index and tree.
    fn service_harness(
        tmp: irys_testing_utils::tempfile::TempDir,
        config: Config,
        module: Arc<StorageModule>,
        path: std::path::PathBuf,
    ) -> Harness {
        // A reopened harness keeps the dropped one's database directory.
        let db_dir = (0..)
            .map(|n| tmp.path().join(format!("chain-db-{n}")))
            .find(|dir| !dir.exists())
            .expect("a free database directory");
        let db_env =
            DatabaseProvider::for_testing(&db_dir, &db_dir.join("cache")).expect("chain db");
        let block_index_guard = BlockIndexReadGuard::new(BlockIndex::new_for_testing(db_env));
        let block_tree = BlockTree::new(&new_mock_signed_header(), config.consensus.clone());
        let block_tree_guard = BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));
        let (service_senders, receivers) = ServiceSenders::new();
        let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();
        let identity = ChainIdentity::new(&config, GENESIS);
        let inner = StorageModuleServiceInner::new(
            Arc::new(RwLock::new(vec![Arc::clone(&module)])),
            block_index_guard,
            block_tree_guard,
            service_senders,
            config,
            identity,
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

    /// A capacity module with entropy at offset 0, closed and reopened without
    /// an assignment, as startup opens every drive. Returns the held hash and
    /// the miner.
    fn parked_harness() -> (Harness, H256, irys_types::IrysAddress) {
        let held = harness(None);
        write_synced_entropy(&held.module);
        let (hash, miner) = assigned_hash_and_miner(&held.module);
        let Harness {
            _tmp: tmp,
            inner,
            module,
            path,
            ..
        } = held;
        let config = inner.config.clone();
        drop(inner);
        drop(module);

        let parked = open_module(&config, &path, None);
        assert!(parked.partition_assignment().is_none());
        assert_eq!(chunk_type(&parked), Some(irys_domain::ChunkType::Entropy));
        (service_harness(tmp, config, parked, path), hash, miner)
    }

    /// A parked module that is later assigned the partition it holds takes it
    /// back without a reset: its entropy stays and is not packed again.
    #[tokio::test]
    async fn parked_module_assigned_its_own_partition_keeps_its_entropy() {
        use irys_domain::storage_module::ChunkType;

        let (mut harness, hash, miner) = parked_harness();
        apply_update(&mut harness, capacity_update(hash, miner), 5).await;
        assert_eq!(
            harness.module.partition_hash(),
            Some(hash),
            "the held partition is installed"
        );
        assert_eq!(
            chunk_type(&harness.module),
            Some(ChunkType::Entropy),
            "taking back the held partition must not reset it"
        );
        assert!(!harness.module.reset_incomplete());
        assert!(!reset_marker(&harness.module));
        while let Ok(request) = harness.receivers.packing.try_recv() {
            let range = request.chunk_range();
            assert!(
                range.0.start() > PartitionChunkOffset::from(0),
                "only Uninitialized offsets are packed again, got {range:?}"
            );
        }
        assert_reopen_chunk(harness, ChunkType::Entropy);
    }

    /// A parked module assigned a different partition is reset and packed.
    #[tokio::test]
    async fn parked_module_assigned_another_partition_resets() {
        use irys_domain::storage_module::ChunkType;

        let (mut harness, hash, miner) = parked_harness();
        let other = H256::repeat_byte(0x7E);
        assert_ne!(other, hash);
        apply_update(&mut harness, capacity_update(other, miner), 5).await;
        assert_eq!(harness.module.partition_hash(), Some(other));
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Uninitialized));
        assert!(harness.receivers.packing.try_recv().is_ok());
    }

    /// The epoch assigns this miner only `hash`, as capacity.
    fn capacity_update(
        hash: H256,
        miner: irys_types::IrysAddress,
    ) -> Arc<Vec<PartitionAssignment>> {
        Arc::new(vec![capacity_assignment(hash, miner)])
    }

    fn capacity_assignment(hash: H256, miner: irys_types::IrysAddress) -> PartitionAssignment {
        PartitionAssignment {
            partition_hash: hash,
            miner_address: miner,
            ledger_id: None,
            slot_index: None,
        }
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

    /// Replace `packing_params.toml` with bytes `from_toml` rejects. The
    /// service judges drives from the module's in-memory record, so it does
    /// not read the file. `reset` does not read it either. `store_assignment`
    /// does, after the wipe.
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

    /// A slot exit whose assignment store fails stops the node, naming the
    /// module and the operation. The old slot's data is already relabelled
    /// and the file still names the old slot, as a crash at that write
    /// leaves it. A reopen that asks for the new slot repeats the exit: the
    /// entropy stays and the old data stays `Uninitialized`.
    #[tokio::test]
    async fn slot_exit_store_failure_is_a_drive_fault_and_reopen_repeats_it() {
        use irys_domain::storage_module::ChunkType;

        let mut harness = harness(Some(DataLedger::Publish.into()));
        write_span(&harness.module, 1, 9, ChunkType::Entropy);
        write_synced_data(&harness.module, 0x57);
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        let path = harness.path.clone();
        let saved = corrupt_packing_params(&path);
        let message = update_fault(&mut harness, capacity_update(hash, miner), 4).await;
        assert!(message.contains("storage module 0"), "{message}");
        assert!(
            message.contains("leave the ledger slot (keep-entropy reset) failed"),
            "{message}"
        );
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Uninitialized));

        restore_packing_params(&path, &saved);
        let (_tmp, config, path, held) = close_for_reopen(harness);
        assert_eq!(held.ledger_id, Some(DataLedger::Publish.into()));
        let capacity = capacity_assignment(hash, miner);
        // Startup opens parked; the service's verdict then repeats the exit.
        let reopened = StorageModule::new(
            &StorageModuleInfo {
                id: 0,
                partition_assignment: None,
                submodules: vec![(partition_chunk_offset_ie!(0, 10), path)],
            },
            &config,
        )
        .expect("reopen parked");
        reopened.set_chain_identity(GENESIS);
        reopened
            .reset_keep_entropy_with_assignment(capacity, 5)
            .expect("the exit repeats");
        assert_eq!(
            reopened.get_chunk_type(&PartitionChunkOffset::from(0)),
            Some(ChunkType::Uninitialized)
        );
        assert_eq!(
            reopened.get_intervals(ChunkType::Entropy),
            vec![irys_types::partition_chunk_offset_ii!(1, 9)],
            "the reopen keeps the entropy"
        );
        let recorded = reopened.recorded_params();
        assert_eq!(recorded.ledger, None, "the reopen stores capacity");
        assert!(!reset_marker(&reopened), "a slot exit writes no marker");
    }

    /// A repeated assignment is not a wipe.
    #[tokio::test]
    async fn unchanged_assignment_keeps_its_bytes() {
        use irys_domain::storage_module::ChunkType;

        let mut harness = harness(Some(DataLedger::Publish.into()));
        write_synced_data(&harness.module, 0x44);
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        harness
            .inner
            .apply_assignment(update_msg(publish_update(hash, miner), 6))
            .await
            .expect("same ledger and slot");
        assert!(!harness.module.reset_incomplete());
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Data));
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            Some(DataLedger::Publish.into())
        );
        assert!(harness.receivers.packing.try_recv().is_err());
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
        write_span(&harness.module, 5, 9, ChunkType::Entropy);
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

        let update = Arc::new(vec![PartitionAssignment {
            partition_hash: hash,
            miner_address: miner,
            ledger_id: Some(DataLedger::Publish.into()),
            slot_index: Some(1),
        }]);
        harness
            .inner
            .apply_assignment(update_msg(update, 8))
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
        assert_eq!(
            harness.module.get_intervals(ChunkType::Entropy),
            vec![irys_types::partition_chunk_offset_ii!(5, 9)],
            "the partition's entropy is kept across the slot change"
        );
        let stored =
            PackingParams::from_toml(harness.path.join(irys_domain::PACKING_PARAMS_FILE_NAME))
                .expect("params");
        assert_eq!(stored.ledger, Some(DataLedger::Publish.into()));
        assert_eq!(stored.slot, Some(1));
        let spans: Vec<_> = drain_packing(&mut harness)
            .iter()
            .map(|r| r.chunk_range().0)
            .collect();
        assert_eq!(
            spans,
            vec![irys_types::partition_chunk_offset_ii!(0, 4)],
            "only the old data span and the unpacked gap are packed"
        );
    }

    /// Apply `assignments` at `height` and return the drive fault the update
    /// returns. The message boundary stops the node on it; the handler under
    /// it returns the fault, so a test checks it without stopping.
    async fn update_fault(
        harness: &mut Harness,
        assignments: Arc<Vec<PartitionAssignment>>,
        height: u64,
    ) -> String {
        message_fault(harness, update_msg(assignments, height)).await
    }

    /// Handle `message` and return the drive fault it returns, as text.
    async fn message_fault(harness: &mut Harness, message: StorageModuleServiceMessage) -> String {
        let err = harness
            .inner
            .handle_message(message)
            .await
            .expect_err("the update hits a drive fault");
        err.downcast_ref::<DriveFault>()
            .unwrap_or_else(|| panic!("not a drive fault: {err:#}"))
            .to_string()
    }

    /// A drive error during a release wipe stops the node with the module,
    /// its path and the operation. Nothing is retried. The injected failure
    /// lands before the reset marker, as a crash there would: a reopen keeps
    /// the assignment and the bytes.
    #[tokio::test]
    async fn release_wipe_failure_is_a_drive_fault() {
        use irys_domain::storage_module::ChunkType;

        let mut harness = harness(Some(DataLedger::Publish.into()));
        write_synced_data(&harness.module, 0x66);
        harness.module.fail_next_reset_for_test();
        let message = update_fault(&mut harness, explicit_unassign(), 4).await;
        assert!(message.contains("storage module 0"), "{message}");
        assert!(
            message.contains(&harness.path.display().to_string()),
            "{message}"
        );
        assert!(
            message.contains("wipe a released partition failed"),
            "{message}"
        );
        assert!(!reset_marker(&harness.module));
        assert_reopen_chunk(harness, ChunkType::Data);
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

    /// The epoch assigns this miner nothing.
    fn explicit_unassign() -> Arc<Vec<PartitionAssignment>> {
        Arc::new(vec![])
    }

    fn publish_slot_update(
        hash: H256,
        miner: irys_types::IrysAddress,
        slot: usize,
    ) -> Arc<Vec<PartitionAssignment>> {
        Arc::new(vec![PartitionAssignment {
            partition_hash: hash,
            miner_address: miner,
            ledger_id: Some(DataLedger::Publish.into()),
            slot_index: Some(slot),
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
        apply_update(&mut harness, publish_slot_update(hash, miner, 1), height).await;
        assert_eq!(stored_height(&harness.path), Some(height));
        assert_eq!(
            harness.module.partition_assignment().unwrap().slot_index,
            Some(1)
        );
        write_synced_data(&harness.module, byte);
        let _packed = drain_packing(&mut harness);
        (harness, hash, miner)
    }

    /// The module holds no assignment in memory (it is not mined or synced
    /// into) and its file still records slot 1 of `hash` at height 50.
    fn assert_parked_on_slot_one(harness: &Harness, hash: H256) {
        assert!(
            harness.module.partition_assignment().is_none(),
            "a drive whose release is not provable is parked"
        );
        let recorded = harness.module.recorded_params();
        assert_eq!(recorded.partition_hash, Some(hash));
        assert_eq!(recorded.slot, Some(1));
        assert_eq!(stored_height(&harness.path), Some(50));
    }

    /// A parked drive (no assignment in memory) whose file records a ledger
    /// slot, given the same partition on another slot, leaves that slot from
    /// its recorded file: the entropy stays and only the old data is packed.
    #[tokio::test]
    async fn parked_drive_leaving_its_recorded_slot_keeps_entropy() {
        use irys_domain::storage_module::ChunkType;

        let (mut harness, hash, miner) = store_slot_one_with_data(50, 0x24).await;
        write_span(&harness.module, 5, 9, ChunkType::Entropy);
        apply_update(&mut harness, explicit_unassign(), 10).await;
        assert_parked_on_slot_one(&harness, hash);

        apply_update(&mut harness, publish_slot_update(hash, miner, 2), 60).await;
        let assignment = harness.module.partition_assignment().expect("assigned");
        assert_eq!(assignment.slot_index, Some(2));
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Uninitialized));
        assert_eq!(
            harness.module.get_intervals(ChunkType::Entropy),
            vec![irys_types::partition_chunk_offset_ii!(5, 9)]
        );
        assert!(!reset_marker(&harness.module));
        assert_eq!(harness.module.recorded_params().slot, Some(2));
        let spans: Vec<_> = drain_packing(&mut harness)
            .iter()
            .map(|request| request.chunk_range().0)
            .collect();
        assert_eq!(spans, vec![irys_types::partition_chunk_offset_ii!(0, 4)]);
    }

    /// An unassign at the stored height is not stale. It still clears and
    /// does not pack an unassigned module.
    #[tokio::test]
    async fn unassign_at_the_stored_height_still_resets() {
        use irys_domain::storage_module::ChunkType;

        let (mut harness, _hash, _miner) = store_slot_one_with_data(50, 0x33).await;
        apply_update(&mut harness, explicit_unassign(), 50).await;
        assert!(harness.module.partition_assignment().is_none());
        assert!(!harness.module.reset_incomplete());
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
        use irys_types::StorageSyncConfig;
        use std::os::unix::fs::FileExt as _;

        let mut harness = open_harness(Some(DataLedger::Publish.into()), |node_config| {
            node_config.storage = StorageSyncConfig {
                num_writes_before_sync: 1,
                intervals_persist_secs: 0,
                min_durable_interval_ms: 0,
                ..StorageSyncConfig::default()
            };
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
        harness.inner.tick().await;
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

    fn publish_update(hash: H256, miner: irys_types::IrysAddress) -> Arc<Vec<PartitionAssignment>> {
        publish_slot_update(hash, miner, 0)
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

    /// A packing-params write error on capacity→ledger stops the node. The
    /// in-memory assignment and the file stay capacity, as a crash at that
    /// write leaves them, and a reopen keeps the bytes.
    #[tokio::test]
    async fn capacity_to_ledger_write_error_is_a_drive_fault() {
        use irys_domain::storage_module::ChunkType;

        let mut harness = harness(None);
        write_synced_entropy(&harness.module);
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        let restore = WritableDir(harness.path.clone());
        set_dir_mode(&harness.path, 0o555);

        let message = update_fault(&mut harness, publish_update(hash, miner), 6).await;
        assert!(message.contains("storage module 0"), "{message}");
        assert!(
            message.contains("store the capacity-to-ledger assignment failed"),
            "{message}"
        );
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            None,
            "a failed write leaves the in-memory assignment unchanged"
        );
        drop(restore);
        assert_reopen_chunk(harness, ChunkType::Entropy);
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
            .apply_assignment(update_msg(publish_update(hash, miner), 7))
            .await
            .expect("capacity to ledger");
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
            "capacity to ledger enqueues no packing"
        );
    }

    fn assigned_hash_and_miner(module: &StorageModule) -> (H256, irys_types::IrysAddress) {
        let assignment = module.partition_assignment().expect("assigned");
        (assignment.partition_hash, assignment.miner_address)
    }

    /// An epoch update at epoch `height`, not in sync.
    fn update_msg(
        assignments: Arc<Vec<PartitionAssignment>>,
        height: u64,
    ) -> StorageModuleServiceMessage {
        StorageModuleServiceMessage::PartitionAssignmentsUpdated {
            assignments,
            epoch_height: height,
            in_sync: false,
        }
    }

    async fn apply_update(
        harness: &mut Harness,
        assignments: Arc<Vec<PartitionAssignment>>,
        height: u64,
    ) {
        harness
            .inner
            .apply_assignment(update_msg(assignments, height))
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
        apply_update(&mut harness, explicit_unassign(), 4).await;
        assert!(harness.module.partition_assignment().is_none());
        harness
            .module
            .requeue_crc_repair_range(repair_one_to_three().0);
        harness
            .module
            .requeue_packing_range(repair_one_to_three().0);
        harness.inner.tick().await;
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
        harness.inner.tick().await;
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
        harness.inner.tick().await;
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
        harness.inner.tick().await;
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
        harness.inner.tick().await;
        let left = harness.module.take_crc_repair_ranges();
        assert!(
            left.is_empty(),
            "a full channel must keep the repair as one range, not put offsets back: {left:?}"
        );
        assert!(
            harness.receivers.packing.try_recv().is_ok(),
            "the filled channel still holds a filler request"
        );
        harness.inner.tick().await;
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
        harness.inner.tick().await;
        assert!(
            harness.receivers.packing.try_recv().is_ok(),
            "one filler leaves so the entropy span can be sent"
        );
        harness.inner.tick().await;
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
        harness.inner.tick().await;
        let _fillers = drain_packing(&mut harness);
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        apply_update(&mut harness, publish_update(hash, miner), 5).await;
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
        harness.inner.tick().await;
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
        harness.inner.tick().await;
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
        harness.inner.tick().await;
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
        harness.inner.tick().await;
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
        harness.inner.tick().await;
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
        harness.inner.tick().await;
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

    /// A reorg that drops a pre-finality activation grows the bound back.
    /// The re-exposed suffix is packed on the next tick, and a full channel
    /// keeps it until a later tick can send it.
    #[tokio::test]
    async fn regrown_suffix_survives_a_full_packing_channel() -> eyre::Result<()> {
        let mut harness = harness(None);
        let module = Arc::clone(&harness.module);
        let span = |request: &PackingRequest| {
            (
                *request.chunk_range().0.start(),
                *request.chunk_range().0.end(),
            )
        };

        module.set_partition_chunks(4)?;
        assert_eq!(
            module.take_regrown_suffix(),
            None,
            "a shrink exposes nothing"
        );
        // Growth clipped by a later shrink keeps only the part still in bound.
        module.set_partition_chunks(10)?;
        module.set_partition_chunks(6)?;
        assert_eq!(
            module.take_regrown_suffix(),
            Some(irys_types::partition_chunk_offset_ii!(4, 5))
        );
        assert_eq!(
            module.take_regrown_suffix(),
            None,
            "the suffix is taken once"
        );

        module.set_partition_chunks(4)?;
        fill_packing_channel(&harness);
        module.set_partition_chunks(10)?;
        harness.inner.tick().await;
        let pending = harness
            .inner
            .pending_ranges
            .get(&0)
            .cloned()
            .unwrap_or_default();
        assert_eq!(pending.len(), 1, "a full channel keeps the regrown suffix");
        assert_eq!(
            (*pending[0].range.0.start(), *pending[0].range.0.end()),
            (4, 9)
        );

        drain_packing(&mut harness);
        harness.inner.tick().await;
        let sent: Vec<_> = drain_packing(&mut harness).iter().map(span).collect();
        assert_eq!(sent, vec![(4, 9)], "the kept suffix is sent once");
        assert!(!harness.inner.pending_ranges.contains_key(&0));
        harness.inner.tick().await;
        assert!(drain_packing(&mut harness).is_empty(), "no repeat send");
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

    /// Ledger→capacity (a slot exit) while the channel is full. The old
    /// slot's span is still `Uninitialized` because the channel never
    /// accepted it.
    async fn defer_slot_exit_pack(harness: &mut Harness) {
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        fill_packing_channel(harness);
        apply_update(harness, capacity_update(hash, miner), 4).await;
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            None
        );
        assert!(
            harness.inner.pending_ranges.contains_key(&0),
            "a full channel keeps the slot-exit span"
        );
        let uninitialized = harness
            .module
            .get_intervals(irys_domain::ChunkType::Uninitialized);
        assert_eq!(
            uninitialized.len(),
            1,
            "the slot exit publishes one uninitialized span"
        );
        assert_eq!(*uninitialized[0].start(), 0);
        assert_eq!(*uninitialized[0].end(), 9);
    }

    /// The channel is still full at the epoch boundary. After it drains, tick
    /// sends the uninitialized span the slot exit could not send.
    #[tokio::test]
    async fn capacity_to_ledger_sends_a_deferred_uninitialized_span() {
        let mut harness = harness(Some(DataLedger::Publish.into()));
        defer_slot_exit_pack(&mut harness).await;
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        apply_update(&mut harness, publish_update(hash, miner), 5).await;
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
        harness.inner.tick().await;
        let request = harness
            .receivers
            .packing
            .try_recv()
            .expect("capacity to ledger must pack a slot-exit span the channel never accepted");
        assert_full_partition_span(&request);
        assert!(!harness.inner.pending_ranges.contains_key(&0));
        assert!(harness.receivers.packing.try_recv().is_err());
    }

    /// The channel has room before the epoch boundary. The update itself sends
    /// the uninitialized span. It does not wait for a later tick.
    #[tokio::test]
    async fn capacity_to_ledger_sends_a_deferred_span_when_the_channel_has_room() {
        let mut harness = harness(Some(DataLedger::Publish.into()));
        defer_slot_exit_pack(&mut harness).await;
        let _fillers = drain_packing(&mut harness);
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        apply_update(&mut harness, publish_update(hash, miner), 5).await;
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
        defer_slot_exit_pack(&mut harness).await;
        write_span(&harness.module, 0, 9, irys_domain::ChunkType::Entropy);
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        apply_update(&mut harness, publish_update(hash, miner), 5).await;
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
        harness.inner.tick().await;
        assert!(
            harness.receivers.packing.try_recv().is_err(),
            "capacity to ledger must not repack entropy"
        );
    }

    /// Data on the deferred span is the ledger's bytes. Do not pack over it.
    #[tokio::test]
    async fn capacity_to_ledger_drops_a_deferred_span_that_is_data() {
        let mut harness = harness(Some(DataLedger::Publish.into()));
        defer_slot_exit_pack(&mut harness).await;
        write_span(&harness.module, 0, 9, irys_domain::ChunkType::Data);
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        apply_update(&mut harness, publish_update(hash, miner), 5).await;
        assert_eq!(
            harness
                .module
                .get_chunk_type(&PartitionChunkOffset::from(0)),
            Some(irys_domain::ChunkType::Data)
        );
        assert!(!harness.inner.pending_ranges.contains_key(&0));
        let _fillers = drain_packing(&mut harness);
        harness.inner.tick().await;
        assert!(
            harness.receivers.packing.try_recv().is_err(),
            "capacity to ledger must not pack over data"
        );
    }

    /// A prefix that became Data stays. Only the uninitialized suffix is sent.
    #[tokio::test]
    async fn capacity_to_ledger_sends_only_the_uninitialized_suffix() {
        let mut harness = harness(Some(DataLedger::Publish.into()));
        defer_slot_exit_pack(&mut harness).await;
        write_span(&harness.module, 0, 4, irys_domain::ChunkType::Data);
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        apply_update(&mut harness, publish_update(hash, miner), 5).await;
        let _fillers = drain_packing(&mut harness);
        harness.inner.tick().await;
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

    fn assert_only_filler_ranges(queued: &[PackingRequest]) {
        assert!(
            queued
                .iter()
                .all(|request| *request.chunk_range().0.end() == 0),
            "a slot-exit span was accepted while the channel was full"
        );
    }

    async fn deferred_span_is_sent_on_tick(harness: &mut Harness) {
        let queued = drain_packing(harness);
        assert_only_filler_ranges(&queued);
        harness.inner.tick().await;
        let request = harness
            .receivers
            .packing
            .try_recv()
            .expect("the slot-exit span is sent once the channel has room");
        assert_full_partition_span(&request);
        assert!(!harness.inner.pending_ranges.contains_key(&0));
    }

    #[tokio::test]
    async fn full_packing_channel_retries_ledger_to_capacity_pack() {
        let mut harness = harness(Some(DataLedger::Publish.into()));
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        fill_packing_channel(&harness);
        apply_update(&mut harness, capacity_update(hash, miner), 4).await;
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            None
        );
        deferred_span_is_sent_on_tick(&mut harness).await;
    }

    #[tokio::test]
    async fn full_packing_channel_retries_ledger_to_ledger_pack() {
        let mut harness = harness(Some(DataLedger::Submit.into()));
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        fill_packing_channel(&harness);
        let update = Arc::new(vec![PartitionAssignment {
            partition_hash: hash,
            miner_address: miner,
            ledger_id: Some(DataLedger::Publish.into()),
            slot_index: Some(1),
        }]);
        apply_update(&mut harness, update, 8).await;
        let assignment = harness.module.partition_assignment().unwrap();
        assert_eq!(assignment.ledger_id, Some(DataLedger::Publish.into()));
        assert_eq!(assignment.slot_index, Some(1));
        deferred_span_is_sent_on_tick(&mut harness).await;
    }

    #[tokio::test]
    async fn full_packing_channel_retries_fresh_assignment_pack() {
        let mut harness = harness(None);
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        fill_packing_channel(&harness);
        apply_update(&mut harness, explicit_unassign(), 4).await;
        assert!(harness.module.partition_assignment().is_none());
        apply_update(&mut harness, capacity_update(hash, miner), 5).await;
        assert!(harness.module.partition_assignment().is_some());
        deferred_span_is_sent_on_tick(&mut harness).await;
    }

    /// After a reorg that orphans the only epoch block, the block tree resends
    /// the new tip's assignments: `epoch_height` is that snapshot's epoch,
    /// older than the orphaned one. The drive was assigned at the orphaned
    /// epoch, so the snapshot cannot prove a release: judged on
    /// `epoch_height` it is parked and keeps its bytes. A later epoch that
    /// still does not assign it releases it.
    #[tokio::test]
    async fn orphaned_epoch_resend_uses_snapshot_height() {
        use irys_domain::storage_module::ChunkType;
        const CANONICAL_EPOCH_HEIGHT: u64 = 40;
        const ORPHANED_EPOCH_HEIGHT: u64 = 50;

        let (mut harness, hash, _miner) =
            store_slot_one_with_data(ORPHANED_EPOCH_HEIGHT, 0xE1).await;
        harness
            .inner
            .apply_assignment(StorageModuleServiceMessage::PartitionAssignmentsUpdated {
                assignments: explicit_unassign(),
                epoch_height: CANONICAL_EPOCH_HEIGHT,
                in_sync: true,
            })
            .await
            .expect("orphaned-epoch resend");
        assert_parked_on_slot_one(&harness, hash);
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Data));
        assert!(!reset_marker(&harness.module));
        assert!(harness.receivers.packing.try_recv().is_err());

        apply_update(&mut harness, explicit_unassign(), 60).await;
        assert!(harness.module.partition_assignment().is_none());
        assert_eq!(
            harness.module.recorded_params().partition_hash,
            None,
            "a release is cleared"
        );
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Uninitialized));
    }

    /// The file of a drive that is ours becomes unreadable while the node
    /// runs. The verdict is judged from the module's in-memory record, so the
    /// drive stays ours and nothing is wiped. The v2 stamp an in-sync epoch
    /// asks for cannot be written, which stops the node naming the module
    /// and the operation.
    #[tokio::test]
    async fn v2_stamp_failure_is_a_drive_fault() {
        use irys_domain::storage_module::ChunkType;

        let mut harness = open_harness_with(Some(DataLedger::Publish.into()), |_| {}, false);
        write_synced_data(&harness.module, 0x61);
        let held = harness.module.partition_assignment().expect("assigned");
        let _corrupted = corrupt_packing_params(&harness.path);

        let message = message_fault(
            &mut harness,
            StorageModuleServiceMessage::PartitionAssignmentsUpdated {
                assignments: Arc::new(vec![held]),
                epoch_height: 5,
                in_sync: true,
            },
        )
        .await;
        assert!(message.contains("storage module 0"), "{message}");
        assert!(
            message.contains("rewrite packing params as v2 failed"),
            "{message}"
        );
        assert_eq!(
            harness.inner.last_decisions.get(&0).map(|(v, _)| v.clone()),
            Some(DriveVerdict::Ours),
            "the verdict comes from memory"
        );
        assert_eq!(harness.module.partition_assignment(), Some(held));
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Data));
    }

    /// Startup opens every drive parked and the service applies the startup
    /// verdict, so a slot change at epoch 2000 is stored by the runtime path
    /// and records 2000 as the trusted assigned height. An older epoch (1500)
    /// asking for the old slot then waits, and a later epoch (3000) that
    /// drops the partition releases the drive.
    #[tokio::test]
    async fn startup_slot_change_records_the_epoch_height() {
        use irys_domain::storage_module::ChunkType;

        let mut harness = harness(Some(DataLedger::Submit.into()));
        write_span(&harness.module, 1, 9, ChunkType::Entropy);
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        assert!(
            harness.module.park_assignment().is_some(),
            "startup opens parked"
        );

        apply_update(&mut harness, publish_slot_update(hash, miner, 0), 2000).await;
        let recorded = harness.module.recorded_params();
        assert_eq!(recorded.ledger, Some(DataLedger::Publish.into()));
        assert_eq!(recorded.trusted_assigned_height(), Some(2000));
        assert_eq!(
            harness.module.get_intervals(ChunkType::Entropy),
            vec![irys_types::partition_chunk_offset_ii!(1, 9)],
            "the slot change keeps the entropy"
        );

        let old_slot = Arc::new(vec![PartitionAssignment {
            partition_hash: hash,
            miner_address: miner,
            ledger_id: Some(DataLedger::Submit.into()),
            slot_index: Some(0),
        }]);
        apply_update(&mut harness, old_slot, 1500).await;
        assert_eq!(
            harness.inner.last_decisions.get(&0).map(|(v, _)| v.clone()),
            Some(DriveVerdict::Wait(
                irys_domain::drive_verdict::WaitReason::RecordedNewerThanEpoch
            )),
            "an older epoch does not move the drive back"
        );
        let recorded = harness.module.recorded_params();
        assert_eq!(recorded.ledger, Some(DataLedger::Publish.into()));
        assert_eq!(recorded.trusted_assigned_height(), Some(2000));

        apply_update(&mut harness, explicit_unassign(), 3000).await;
        assert_eq!(
            harness.inner.last_decisions.get(&0).map(|(v, _)| v.clone()),
            Some(DriveVerdict::Released)
        );
        assert!(harness.module.partition_assignment().is_none());
        assert_eq!(
            harness.module.recorded_params().partition_hash,
            None,
            "the released drive records no partition"
        );
    }

    /// A ledger exit at the canonical epoch applies at once: capacity
    /// installed, the partition's entropy kept, and only the old slot's data
    /// span uninitialized and packed.
    #[tokio::test]
    async fn ledger_exit_keeps_entropy_and_installs_capacity() {
        use irys_domain::storage_module::ChunkType;

        let mut harness = harness(Some(DataLedger::Publish.into()));
        write_span(&harness.module, 4, 9, ChunkType::Entropy);
        write_synced_data(&harness.module, 0x62);
        let (hash, miner) = assigned_hash_and_miner(&harness.module);

        apply_update(&mut harness, capacity_update(hash, miner), 5).await;
        let assignment = harness.module.partition_assignment().expect("capacity");
        assert_eq!(assignment.partition_hash, hash);
        assert_eq!(assignment.ledger_id, None);
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Uninitialized));
        assert_eq!(
            harness.module.get_intervals(ChunkType::Entropy),
            vec![irys_types::partition_chunk_offset_ii!(4, 9)],
            "the partition's entropy is kept"
        );
        assert!(!reset_marker(&harness.module));
        let recorded = harness.module.recorded_params();
        assert_eq!(recorded.ledger, None);
        assert_eq!(recorded.trusted_assigned_height(), Some(5));
        let requests = drain_packing(&mut harness);
        let spans: Vec<_> = requests.iter().map(|r| r.chunk_range().0).collect();
        assert_eq!(
            spans,
            vec![irys_types::partition_chunk_offset_ii!(0, 3)],
            "only the uninitialized span is packed, not the whole partition"
        );
    }

    /// A drive whose partition the chain released (v2 file, same chain) is
    /// wiped and records no partition, so it is Empty and receives the next
    /// unheld assignment.
    #[tokio::test]
    async fn released_is_cleared_and_becomes_empty() {
        use irys_domain::storage_module::ChunkType;

        let mut harness = harness(Some(DataLedger::Publish.into()));
        write_synced_data(&harness.module, 0x64);
        let (_hash, miner) = assigned_hash_and_miner(&harness.module);

        apply_update(&mut harness, explicit_unassign(), 5).await;
        assert!(harness.module.partition_assignment().is_none());
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Uninitialized));
        assert!(!reset_marker(&harness.module));
        let recorded = harness.module.recorded_params();
        assert_eq!(recorded.partition_hash, None);
        assert_eq!(recorded.ledger, None);
        assert!(recorded.is_v2());
        assert_eq!(recorded.genesis_hash, Some(GENESIS));
        assert!(harness.receivers.packing.try_recv().is_err());

        let other = H256::repeat_byte(0x7D);
        apply_update(&mut harness, capacity_update(other, miner), 6).await;
        assert_eq!(harness.module.partition_hash(), Some(other));
        let request = harness
            .receivers
            .packing
            .try_recv()
            .expect("the empty drive packs its new partition");
        assert_full_partition_span(&request);
    }

    /// A v1 drive that is ours is rewritten as v2 only at an epoch processed
    /// in sync, with `assigned_height` = that epoch. Bytes stay.
    #[tokio::test]
    async fn in_sync_epoch_stamps_v1_ours_drive() {
        use irys_domain::storage_module::ChunkType;

        let mut harness = open_harness_with(Some(DataLedger::Publish.into()), |_| {}, false);
        write_synced_data(&harness.module, 0x65);
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        let params_path = harness.path.join(irys_domain::PACKING_PARAMS_FILE_NAME);
        assert!(!PackingParams::from_toml(&params_path).expect("v1").is_v2());

        apply_update(&mut harness, publish_update(hash, miner), 7).await;
        assert!(
            !PackingParams::from_toml(&params_path).expect("v1").is_v2(),
            "an epoch processed while syncing does not stamp"
        );

        harness
            .inner
            .apply_assignment(StorageModuleServiceMessage::PartitionAssignmentsUpdated {
                assignments: publish_update(hash, miner),
                epoch_height: 7,
                in_sync: true,
            })
            .await
            .expect("in-sync epoch");
        let params = PackingParams::from_toml(&params_path).expect("v2");
        assert!(params.is_v2());
        assert_eq!(params.assigned_height, Some(7));
        assert_eq!(params.genesis_hash, Some(GENESIS));
        assert_eq!(
            params.chain_id,
            Some(harness.inner.config.consensus.chain_id)
        );
        assert_eq!(params.partition_hash, Some(hash));
        assert_eq!(params.slot, Some(0));
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Data));
        assert!(harness.receivers.packing.try_recv().is_err());
    }

    /// The orphaned-epoch resend shape for a ledger exit: the drive recorded
    /// slot 1 of Publish at epoch 50, and the snapshot at epoch 40 says
    /// capacity. The drive's own state is newer than that epoch, so it is
    /// parked with its bytes, labels and file intact.
    #[tokio::test]
    async fn ledger_exit_against_an_older_epoch_parks_the_drive() {
        use irys_domain::storage_module::ChunkType;
        const CANONICAL_EPOCH_HEIGHT: u64 = 40;
        const ORPHANED_EPOCH_HEIGHT: u64 = 50;

        let (mut harness, hash, miner) =
            store_slot_one_with_data(ORPHANED_EPOCH_HEIGHT, 0xE2).await;
        let params_path = harness.path.join(irys_domain::PACKING_PARAMS_FILE_NAME);
        let file_before = std::fs::read(&params_path).expect("file");
        harness
            .inner
            .apply_assignment(StorageModuleServiceMessage::PartitionAssignmentsUpdated {
                assignments: capacity_update(hash, miner),
                epoch_height: CANONICAL_EPOCH_HEIGHT,
                in_sync: true,
            })
            .await
            .expect("orphaned-epoch resend");
        assert_parked_on_slot_one(&harness, hash);
        assert_eq!(chunk_type(&harness.module), Some(ChunkType::Data));
        assert!(!harness.module.reset_incomplete());
        assert!(!reset_marker(&harness.module));
        assert!(harness.receivers.packing.try_recv().is_err());
        assert_eq!(std::fs::read(&params_path).expect("file"), file_before);
        assert_eq!(
            harness
                .inner
                .last_decisions
                .get(&0)
                .map(|(v, _)| v.reason()),
            Some("recorded state is newer than the epoch".to_owned())
        );
    }

    /// A later update with the same ledger and slot must not drop a span the
    /// channel has not accepted yet.
    #[tokio::test]
    async fn same_assignment_keeps_a_deferred_slot_exit_pack() {
        let mut harness = harness(Some(DataLedger::Publish.into()));
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        fill_packing_channel(&harness);
        apply_update(&mut harness, capacity_update(hash, miner), 4).await;
        apply_update(&mut harness, capacity_update(hash, miner), 5).await;
        assert_eq!(
            harness.module.partition_assignment().unwrap().ledger_id,
            None
        );
        deferred_span_is_sent_on_tick(&mut harness).await;
    }

    /// Unassign drops a span the channel did not accept. The module cannot pack.
    #[tokio::test]
    async fn unassign_drops_a_deferred_slot_exit_pack() {
        let mut harness = harness(Some(DataLedger::Publish.into()));
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        fill_packing_channel(&harness);
        apply_update(&mut harness, capacity_update(hash, miner), 4).await;
        assert!(harness.inner.pending_ranges.contains_key(&0));
        apply_update(&mut harness, explicit_unassign(), 5).await;
        assert!(harness.module.partition_assignment().is_none());
        assert!(
            !harness.inner.pending_ranges.contains_key(&0),
            "unassign must drop a span the channel did not accept"
        );
        let _fillers = drain_packing(&mut harness);
        harness.inner.tick().await;
        assert!(harness.receivers.packing.try_recv().is_err());
    }

    #[tokio::test]
    async fn closed_packing_channel_remembers_the_slot_exit_pack() {
        let mut harness = harness(Some(DataLedger::Publish.into()));
        let (hash, miner) = assigned_hash_and_miner(&harness.module);
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        drop(std::mem::replace(&mut harness.receivers.packing, rx));
        apply_update(&mut harness, capacity_update(hash, miner), 4).await;
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
            "a closed channel must remember the slot-exit span"
        );
        assert_eq!(*pending[0].range.0.start(), 0);
        assert_eq!(*pending[0].range.0.end(), 9);
        harness.inner.tick().await;
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
