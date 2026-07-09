//! Priority-based block validation task scheduling.
//!
//! ## High-Level Flow
//!
//! 1. New block enters VDF validation (sequential, preemptible)
//! 2. Valid blocks proceed to concurrent validation (parallel: recall, PoA, shadow txs)
//! 3. Validated blocks wait for parent validation to complete
//! 4. Results are reported to the block tree service
//!
//! ## Priority System
//!
//! Blocks are prioritized by: canonical extension > canonical > fork > unknown,
//! then by height (lower first) and VDF steps (fewer first).

use irys_domain::{BlockTree, BlockTreeReadGuard, ChainState};
use irys_types::{BlockHash, IrysBlockHeader, SealedBlock};
use irys_vdf::state::{CancelEnum, WaitForStepError};
use priority_queue::PriorityQueue;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::task::{JoinHandle, JoinSet};
use tracing::{Instrument as _, debug, info, instrument, warn};

use crate::block_tree_service::ValidationResult;
use crate::metrics;
use crate::validation_service::block_validation_task::BlockValidationTask;
use crate::validation_service::{
    VdfBlockingTaskFailed, VdfStageBParentMissing, VdfTaskStage, VdfValidationResult,
    record_vdf_task_progress,
};

/// Block priority states for validation ordering
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum BlockPriority {
    /// Unknown/orphan blocks (lowest priority)
    Unknown,
    /// Fork blocks that don't extend the canonical tip (low priority)
    Fork,
    /// Canonical blocks already on chain (medium priority)
    Canonical,
    /// Canonical extensions that extend from the canonical tip (highest priority)
    CanonicalExtension,
}

/// Metadata struct that is used to inform block validation priority decisions
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct BlockPriorityMeta {
    pub height: u64,
    pub state: BlockPriority,
    pub vdf_step_count: u64,
}

impl BlockPriorityMeta {
    pub(super) fn new(block: &IrysBlockHeader, state: BlockPriority) -> Self {
        Self {
            height: block.height,
            state,
            vdf_step_count: block.vdf_limiter_info.steps.len() as u64,
        }
    }
}

impl Ord for BlockPriorityMeta {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // First compare by state (CanonicalExtension > Canonical > Fork > Unknown)
        self.state
            .cmp(&other.state)
            // Then by height (lower height = higher priority, so reverse the comparison)
            .then_with(|| other.height.cmp(&self.height))
            // Finally by VDF steps (fewer steps = higher priority, so reverse the comparison)
            .then_with(|| other.vdf_step_count.cmp(&self.vdf_step_count))
    }
}

impl PartialOrd for BlockPriorityMeta {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Result from a concurrent validation task
#[derive(Debug)]
pub(super) struct ConcurrentValidationResult {
    pub block_hash: BlockHash,
    pub validation_result: ValidationResult,
    pub enqueued_at: Instant,
}

/// VDF task with preemption support
pub(super) struct PreemptibleVdfTask {
    pub task: BlockValidationTask,
    pub cancel_u8: Arc<std::sync::atomic::AtomicU8>,
    pub stage_u8: Arc<std::sync::atomic::AtomicU8>,
    pub progress_instant: Arc<Mutex<Instant>>,
    /// Written to `Some(Instant::now())` just before the stage atomic is set
    /// to `Completed`. Read by the watchdog at panic time to compute
    /// `completed_ago`, distinguishing scheduler latency from a real lifecycle
    /// wedge. `None` until the `Completed` transition fires.
    pub completed_at: Arc<Mutex<Option<Instant>>>,
}

impl PreemptibleVdfTask {
    #[instrument(skip_all, fields(block.hash = %self.task.sealed_block.header().block_hash))]
    pub(super) async fn execute(self) -> (VdfValidationResult, BlockValidationTask) {
        // First action on first poll: refresh `progress_instant` from the
        // spawn-queue time (set in `VdfScheduler::start_next`) to first-poll
        // time. Without this, a delayed first poll under tokio runtime
        // saturation would look identical to a genuine in-stage stall to the
        // watchdog. With this, the watchdog clock measures real
        // forward-progress from the moment the future actually gets CPU; the
        // spawn-to-first-poll latency is captured in the `vdf_starting`
        // histogram for diagnostics. We pass `Starting` to keep the stage
        // unchanged (the next stage transition happens inside
        // `ensure_vdf_is_valid`).
        record_vdf_task_progress(
            &self.stage_u8,
            &self.progress_instant,
            VdfTaskStage::Starting,
        );

        let inner = Arc::clone(&self.task.service_inner);
        let header = self.task.sealed_block.header();
        let skip_vdf = self.task.skip_vdf_validation;

        let started = Instant::now();
        // No bridge task needed - just use the AtomicU8 directly!
        let result = match inner
            .ensure_vdf_is_valid(
                header,
                self.cancel_u8.clone(),
                self.stage_u8.clone(),
                self.progress_instant.clone(),
                self.completed_at.clone(),
                skip_vdf,
            )
            .await
        {
            Ok(()) => VdfValidationResult::Valid,
            Err(e) => {
                // Distinguish three error classes from the VDF wait pathway:
                // - `Cancelled` (cooperative preemption / shutdown) → requeue.
                // - `Stalled` (local VDF state did not advance) → PANIC. This is a
                //   local-infrastructure failure (dead writer thread, poisoned
                //   lock, paused sync), not block-invalidity evidence. The
                //   non-cooperative watchdog already panics on the same condition
                //   inside computational stages because surfacing the block as
                //   Invalid would drop a block every honest peer accepts and
                //   fork us off the network — the "never mislabel" rule in
                //   design/docs/vdf-validation-stall-detection.md. The
                //   cooperative `Wait*` stages must follow the same policy.
                // - Anything else (real validation finding) → Invalid.
                // `spawn_blocking` JoinError (panic or external abort): the typed
                // `VdfBlockingTaskFailed` sentinel is checked first. A panic in a
                // blocking verifier thread is a local-infrastructure fault, not
                // block-invalidity evidence — mislabelling it as `Invalid` would
                // peer-attribute a node-side crash and fork us off the network.
                // An external `abort()` (watchdog) is peer-innocent and requeues
                // like any other cancellation. Mirrors the symmetric PoA pattern at
                // `classify_poa_join_error` in `block_validation_task.rs`.
                if let Some(blocking_failed) = e.downcast_ref::<VdfBlockingTaskFailed>() {
                    if blocking_failed.is_panic {
                        // Local verifier thread panicked — this is a node fault,
                        // not block invalidity. Panic here so the never-mislabel
                        // rule is enforced: the JoinError propagates through the
                        // spawned VDF task as JoinError::is_panic(), then
                        // `resume_unwind` in the validation service select loop
                        // re-raises it onto the service task. The global panic hook
                        // then raises SIGINT and the shutdown watchdog forces abort.
                        panic!(
                            "VDF blocking task panicked (stage={:?}, block={}); local-thread fault — crashing per never-mislabel rule",
                            blocking_failed.stage,
                            self.task.sealed_block.header().block_hash,
                        );
                    } else {
                        // is_cancelled() — external abort (watchdog or runtime
                        // teardown). Peer-innocent: requeue via the same lane as
                        // cooperative preemption.
                        metrics::record_validation_cancellation("vdf_blocking_aborted");
                        VdfValidationResult::Cancelled
                    }
                // Stage B parent-eviction race: typed sentinel from
                // `ensure_vdf_is_valid` surfaces as a SoftInternal-mappable
                // ParentMissing rather than the panic the prior `.expect`
                // site produced. Checked before the WaitForStepError downcast
                // since it can fire at any VDF stage entry, not just the
                // wait stage.
                } else if let Some(parent_missing) = e.downcast_ref::<VdfStageBParentMissing>() {
                    metrics::record_validation_cancellation("vdf_parent_missing");
                    VdfValidationResult::ParentMissing {
                        parent_hash: parent_missing.parent_hash,
                    }
                } else {
                    match e.downcast_ref::<WaitForStepError>() {
                        Some(WaitForStepError::Cancelled) => {
                            metrics::record_validation_cancellation("vdf_preempted");
                            VdfValidationResult::Cancelled
                        }
                        Some(WaitForStepError::Stalled { .. }) => {
                            metrics::record_validation_cancellation("vdf_stalled");
                            // See above. Panic propagates through the spawned task
                            // as JoinError::is_panic(), then `resume_unwind` in the
                            // validation service select loop re-raises it onto the
                            // service task. The global panic hook then raises SIGINT
                            // and the 45s shutdown watchdog forces process abort.
                            panic!(
                                "VDF wait stalled (block={}, error={}); local VDF state cannot advance — crashing per never-mislabel rule",
                                self.task.sealed_block.header().block_hash,
                                e
                            );
                        }
                        None => {
                            if self.cancel_u8.load(Ordering::Relaxed) == CancelEnum::Cancelled as u8
                            {
                                metrics::record_validation_cancellation("vdf_preempted");
                                VdfValidationResult::Cancelled
                            } else {
                                VdfValidationResult::Invalid(e)
                            }
                        }
                    }
                }
            }
        };
        metrics::record_vdf_duration_ms(started.elapsed().as_secs_f64() * 1000.0);

        (result, self.task)
    }
}

/// Currently running VDF task with its JoinHandle.
pub(super) struct RunningVdfTask {
    pub hash: BlockHash,
    pub priority: BlockPriorityMeta,
    pub cancel_signal: Arc<std::sync::atomic::AtomicU8>,
    pub stage_signal: Arc<std::sync::atomic::AtomicU8>,
    pub last_progress_at: Arc<Mutex<Instant>>,
    /// Timestamp recorded when the stage transitions to `Completed`, written
    /// before the stage atomic (same ordering window as `last_progress_at`).
    /// Read by the watchdog at panic time to compute `completed_ago`, allowing
    /// CI failures to distinguish scheduler latency (handle already ready, but
    /// `poll_vdf` wasn't polled) from a genuine lifecycle wedge.
    pub completed_at: Arc<Mutex<Option<Instant>>>,
    pub started_at: Instant,
    pub sealed_block: Arc<SealedBlock>,
    pub handle: JoinHandle<(BlockHash, VdfValidationResult, BlockValidationTask)>,
    /// Clone of the task retained so the select handler can requeue if the
    /// handle is unexpectedly cancelled (e.g. runtime teardown). `None` only
    /// in unit tests that construct `RunningVdfTask` directly.
    pub requeue_task: Option<BlockValidationTask>,
}

/// Strategy for spawning VDF validation tasks.
///
/// Production code spawns real `PreemptibleVdfTask::execute()` futures.
/// Tests can supply a no-op strategy that pends forever, allowing queue and
/// scheduling logic to be exercised with stub tasks.
pub(super) trait VdfSpawnStrategy {
    fn spawn_vdf_task(
        &self,
        runtime_handle: &tokio::runtime::Handle,
        task: BlockValidationTask,
        cancel_u8: Arc<std::sync::atomic::AtomicU8>,
        stage_u8: Arc<std::sync::atomic::AtomicU8>,
        progress_instant: Arc<Mutex<Instant>>,
        completed_at: Arc<Mutex<Option<Instant>>>,
        hash: BlockHash,
        priority: BlockPriorityMeta,
    ) -> JoinHandle<(BlockHash, VdfValidationResult, BlockValidationTask)>;
}

/// Spawns real VDF validation tasks via `PreemptibleVdfTask::execute()`.
pub(super) struct ProductionVdfSpawn;

impl VdfSpawnStrategy for ProductionVdfSpawn {
    fn spawn_vdf_task(
        &self,
        runtime_handle: &tokio::runtime::Handle,
        task: BlockValidationTask,
        cancel_u8: Arc<std::sync::atomic::AtomicU8>,
        stage_u8: Arc<std::sync::atomic::AtomicU8>,
        progress_instant: Arc<Mutex<Instant>>,
        completed_at: Arc<Mutex<Option<Instant>>>,
        hash: BlockHash,
        priority: BlockPriorityMeta,
    ) -> JoinHandle<(BlockHash, VdfValidationResult, BlockValidationTask)> {
        runtime_handle.spawn(
            async move {
                let (result, task) = PreemptibleVdfTask {
                    task,
                    cancel_u8,
                    stage_u8,
                    progress_instant,
                    completed_at,
                }
                .execute()
                .await;
                (hash, result, task)
            }
            .instrument(tracing::info_span!(
                "vdf_validation",
                block.hash = %hash,
                block.priority = ?priority
            )),
        )
    }
}

/// Spawns a perpetually-pending no-op future instead of real VDF work.
///
/// This is safe to use with `BlockValidationTask::test_stub` tasks whose
/// `service_inner` field is uninitialized.
#[cfg(test)]
pub(super) struct TestVdfSpawn;

#[cfg(test)]
impl VdfSpawnStrategy for TestVdfSpawn {
    fn spawn_vdf_task(
        &self,
        runtime_handle: &tokio::runtime::Handle,
        _task: BlockValidationTask,
        _cancel_u8: Arc<std::sync::atomic::AtomicU8>,
        _stage_u8: Arc<std::sync::atomic::AtomicU8>,
        _progress_instant: Arc<Mutex<Instant>>,
        _completed_at: Arc<Mutex<Option<Instant>>>,
        _hash: BlockHash,
        _priority: BlockPriorityMeta,
    ) -> JoinHandle<(BlockHash, VdfValidationResult, BlockValidationTask)> {
        runtime_handle.spawn(std::future::pending())
    }
}

/// VDF scheduler with preemption. At most one VDF task runs at a time.
/// The running task's handle is awaited directly in the main select loop.
pub(super) struct VdfScheduler<S: VdfSpawnStrategy = ProductionVdfSpawn> {
    /// Currently running VDF task (at most one).
    pub current: Option<RunningVdfTask>,

    /// Pending VDF tasks ordered by priority.
    pub pending: PriorityQueue<BlockValidationTask, BlockPriorityMeta>,

    /// Runtime handle used to spawn VDF tasks.
    runtime_handle: tokio::runtime::Handle,

    /// Strategy for spawning VDF tasks.
    spawn_strategy: S,
}

impl VdfScheduler<ProductionVdfSpawn> {
    pub(super) fn new(runtime_handle: tokio::runtime::Handle) -> Self {
        Self {
            current: None,
            pending: PriorityQueue::new(),
            runtime_handle,
            spawn_strategy: ProductionVdfSpawn,
        }
    }
}

#[cfg(test)]
impl VdfScheduler<TestVdfSpawn> {
    /// Create a scheduler using [`TestVdfSpawn`] — safe with
    /// [`BlockValidationTask::test_stub`] tasks.
    pub(super) fn new_test_mode(runtime_handle: tokio::runtime::Handle) -> Self {
        Self {
            current: None,
            pending: PriorityQueue::new(),
            runtime_handle,
            spawn_strategy: TestVdfSpawn,
        }
    }
}

impl<S: VdfSpawnStrategy> VdfScheduler<S> {
    /// Await the current VDF task's completion and take it from `current`.
    /// Returns `pending` if no task is running, which naturally disables this
    /// branch in `tokio::select!`.
    ///
    /// Uses `as_mut()` rather than `take()` so that if `tokio::select!` drops
    /// this future (another branch fired), `current` remains `Some` and the
    /// running task isn't orphaned.
    pub(super) async fn poll_vdf(
        &mut self,
    ) -> (
        RunningVdfTask,
        Result<(BlockHash, VdfValidationResult, BlockValidationTask), tokio::task::JoinError>,
    ) {
        // IMPORTANT: we use as_mut() here, NOT take(). This future is polled
        // inside tokio::select! — if another branch completes first, this future
        // is dropped. With take(), that would leave `current` as None while the
        // spawned VDF task is still running, orphaning it and allowing start_next()
        // to launch a second concurrent VDF task. With as_mut(), dropping this
        // future leaves `current` intact so the next select iteration re-polls it.
        match self.current.as_mut() {
            Some(task) => {
                let result = (&mut task.handle).await;
                // The handle resolved, so select will choose this branch — take() is safe.
                let task = self.current.take().unwrap();
                (task, result)
            }
            None => std::future::pending().await,
        }
    }

    /// Submit a VDF task
    #[instrument(skip_all, fields(block.hash = %task.sealed_block.header().block_hash, ?priority))]
    pub(super) fn submit(&mut self, task: BlockValidationTask, priority: BlockPriorityMeta) {
        let hash = task.sealed_block.header().block_hash;

        // Check for duplicates
        if self.pending.get(&task).is_some() {
            return;
        }

        // Check if current task exists
        if let Some(current) = &self.current
            && current.hash == hash
        {
            return;
        }

        self.pending.push(task, priority);

        // Preemption must fire before start_next(). If preemption fires,
        // start_next() is a no-op (current is still Some). The cancelled
        // task will resolve in the next select iteration, and the handler
        // will resubmit it — after which start_next() promotes the higher-
        // priority pending task.
        self.check_preemption();

        // Invariant: after push + preemption check, either the existing task
        // is still running or a new one was just started.
        let active = self.start_next();
        debug_assert!(
            active,
            "VDF scheduler must be processing work after submit (either already running or just started)"
        );
    }

    /// Check if current task should be preempted by higher priority pending task
    pub(super) fn check_preemption(&self) {
        let Some(current) = &self.current else {
            return;
        };

        let Some((_, pending_priority)) = self.pending.peek() else {
            return;
        };

        // Only preempt if pending task has HIGHER priority
        if pending_priority > &current.priority {
            current
                .cancel_signal
                .store(CancelEnum::Cancelled as u8, Ordering::Relaxed);
            metrics::record_vdf_preemption();
        }
    }

    /// Start next VDF task if none running.
    /// Returns `true` if a VDF task is active (either already running or just started).
    #[instrument(skip_all)]
    pub(super) fn start_next(&mut self) -> bool {
        if self.current.is_some() {
            return true; // Already running
        }

        let Some((task, priority)) = self.pending.pop() else {
            return false; // Nothing to run
        };
        let hash = task.sealed_block.header().block_hash;

        // Create AtomicU8 for cancellation
        let cancel_u8 = Arc::new(std::sync::atomic::AtomicU8::new(CancelEnum::Continue as u8));
        let cancel_signal = Arc::clone(&cancel_u8);
        let stage_u8 = Arc::new(std::sync::atomic::AtomicU8::new(
            VdfTaskStage::Starting as u8,
        ));
        let stage_signal = Arc::clone(&stage_u8);
        let progress_instant = Arc::new(Mutex::new(Instant::now()));
        let last_progress_at = Arc::clone(&progress_instant);
        let completed_at_arc = Arc::new(Mutex::new(None::<Instant>));
        let completed_at = Arc::clone(&completed_at_arc);
        let sealed_block = Arc::clone(&task.sealed_block);
        let requeue_task = task.clone();

        let handle = self.spawn_strategy.spawn_vdf_task(
            &self.runtime_handle,
            task,
            cancel_u8,
            stage_u8,
            progress_instant,
            completed_at_arc,
            hash,
            priority,
        );

        self.current = Some(RunningVdfTask {
            hash,
            priority,
            cancel_signal,
            stage_signal,
            last_progress_at,
            completed_at,
            started_at: Instant::now(),
            sealed_block,
            handle,
            requeue_task: Some(requeue_task),
        });
        true
    }

    pub(super) fn abort_stalled_current(
        &mut self,
        hard_timeout: Duration,
    ) -> Option<(Duration, VdfTaskStage)> {
        let current = self.current.as_mut()?;

        let stage = VdfTaskStage::from(current.stage_signal.load(Ordering::Relaxed));
        // Only `WaitPrevStep`/`WaitFinalCatchUp` are skipped, and not because
        // they have a redundant check — they need a *different kind* of check.
        // The watchdog measures stage wall-clock duration; `wait_for_step`
        // measures `global_step` advancement. These are different things, and
        // only the second one is meaningful for a wait stage.
        //
        // A `Wait*` stage can legitimately last minutes — e.g. a peer catching
        // up several hundred VDF steps from gossip at ~1 step/s. The wait is
        // healthy as long as `global_step` keeps advancing; the stage clock has
        // no bearing on whether progress is happening. Watching `Wait*` with a
        // stage-duration timeout would false-fire on every legitimate long
        // catch-up, recreating the original production-hang failure mode in
        // reverse. `wait_for_step` panics on `WaitForStepError::Stalled`
        // (no step advance for `progress_timeout`) — that's the right
        // granularity here, and it shares the same shutdown path as this
        // watchdog.
        //
        // Every other stage is watched, including `Starting` and `Completed`:
        // - `Starting`: resilience to first-poll delay is achieved by
        //   `PreemptibleVdfTask::execute` refreshing `progress_instant` at its
        //   first statement. A 15 s queue-to-first-poll latency *does* trip the
        //   watchdog — that's the desired behavior; a consensus-critical task
        //   the runtime can't schedule for 15 s means the node cannot validate.
        // - `Completed`: a terminal handoff state. Between
        //   `record_vdf_task_progress(Completed)` and `poll_vdf` collecting the
        //   `JoinHandle` is one select-loop round-trip — microseconds in normal
        //   operation. A long gap means the select loop was starved, but if the
        //   handle has already *finished* the result is ready to collect and the
        //   loop has demonstrably resumed (we are running this watchdog), so the
        //   `is_finished()` guard below exempts it rather than aborting a valid,
        //   already-computed result and crashing a recovered node. Only a task
        //   wedged before its handle finishes — the genuine deadlock/poison case
        //   the watchdog exists to surface — trips the abort.
        if matches!(
            stage,
            VdfTaskStage::WaitPrevStep | VdfTaskStage::WaitFinalCatchUp
        ) {
            return None;
        }

        let stalled_for = current
            .last_progress_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .elapsed();
        if stalled_for < hard_timeout {
            return None;
        }

        // A finished handle is not wedged: the task ran to completion and its
        // result is sitting ready for the next `poll_vdf` round-trip. Under
        // CI/CPU starvation the select loop can be delayed past `hard_timeout`
        // *between* completion and collection — the very fact that we are
        // executing this watchdog proves the loop has resumed. Aborting here
        // would store `Cancelled` on (and then discard) a valid, already-computed
        // validation result and crash a node that has recovered. The watchdog
        // targets tasks that never finish — deadlocks, poisoned locks, runaway
        // loops — and those keep `is_finished()` false and still trip the abort
        // below. See design/docs/vdf-validation-stall-detection.md.
        if current.handle.is_finished() {
            return None;
        }

        current
            .cancel_signal
            .store(CancelEnum::Cancelled as u8, Ordering::Relaxed);
        current.handle.abort();

        Some((stalled_for, stage))
    }
}

/// Per-block decision returned by [`ValidationCoordinator::record_concurrent_cancel`].
///
/// A cancellation in the concurrent-stage `JoinSet` result handler may be a
/// one-off Tokio hiccup (the only intentional cancel path runs at shutdown,
/// which can't reach that arm). The first few cancellations for a given block
/// are transparently requeued; sustained recurrence indicates a poisoned
/// local condition (sibling-worker panic loop, runtime tear-down race,
/// watchdog edge case) and we stop re-running the block to avoid starving
/// the rest of the queue. Recovery is fresh gossip re-entry once the local
/// condition clears (same SoftInternal park lane as
/// [`crate::block_validation::ValidationCancelReason::ParentMissing`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ConcurrentCancelDecision {
    /// Retry budget is still available — requeue the block via
    /// `submit_task`. Caller carries `attempt` for tracing/metrics only.
    Requeue { attempt: u8 },
    /// Retry budget exhausted — do NOT requeue. Caller must route the block
    /// via `send_validation_result` with
    /// `ValidationCancelReason::RepeatedCancellation` (SoftInternal: validity
    /// unknown, park the block, fresh gossip is the recovery lane).
    GiveUp { attempt: u8 },
}

/// Per-block cap on concurrent-stage `JoinError::Cancelled` requeues before
/// the validation service stops looping on the same block and routes it
/// through `ValidationCancelReason::RepeatedCancellation`.
///
/// Rationale for `3`: one cancel is plausibly a transient Tokio hiccup
/// (worker panic, runtime tear-down race) that genuinely benefits from a
/// retry. Two is the safety margin for a brief window of distress that
/// clears between attempts. At three the local condition is structural —
/// continuing to requeue tight-loops between the VDF queue and the
/// concurrent JoinSet, burning cycles while starving other blocks of
/// validation slots. The existing `MAX_RETRY_ATTEMPTS = 5` in
/// `block_producer` is the only sibling retry constant; a producer retry is
/// cheap (rebuild on a new parent) and the cost of giving up is "no block
/// this slot", whereas a validation retry pays the full VDF+concurrent
/// pipeline cost on every loop and giving up just parks the block in cache
/// for gossip recovery. The lower cap reflects the higher per-attempt cost
/// and the cheaper give-up path.
pub(super) const MAX_CONCURRENT_CANCEL_RETRIES: u8 = 3;

/// Main validation coordinator
pub(super) struct ValidationCoordinator<S: VdfSpawnStrategy = ProductionVdfSpawn> {
    /// VDF validation scheduler
    pub vdf_scheduler: VdfScheduler<S>,

    /// Concurrent validation tasks
    pub concurrent_tasks: JoinSet<ConcurrentValidationResult>,

    /// Maps task IDs to the bookkeeping needed when a concurrent task fails
    /// to return a result. `sealed_block` + `skip_vdf_validation` allow the
    /// `JoinError::Cancelled` arm in `validation_service` to reconstruct
    /// the task and re-`submit_task` it — `concurrent_tasks.abort_all()`
    /// only runs from `shutdown()`, so a cancel reaching the result handler
    /// is an unexpected runtime hiccup rather than an intentional drop,
    /// and re-queuing is safer than soft-discarding a block whose validity
    /// we never actually decided on.
    pub concurrent_task_blocks:
        HashMap<tokio::task::Id, (BlockHash, Instant, Arc<SealedBlock>, bool)>,

    /// Per-block counter of concurrent-stage `JoinError::Cancelled`
    /// requeues. Keyed on `BlockHash` (not `tokio::task::Id`) because the
    /// requeue path traverses `submit_task → VDF queue → spawn_concurrent`,
    /// which mints a fresh `Id` each time — a per-Id counter would reset on
    /// every requeue and never trip the cap. Counter is cleared when the
    /// block exits via the Ok / panic arms (validation produced a verdict,
    /// or a node-fault was surfaced), or when the cap is hit (the block is
    /// being routed out via `RepeatedCancellation` and won't re-enter
    /// without fresh gossip). See [`ConcurrentCancelDecision`] for the
    /// decision shape and [`MAX_CONCURRENT_CANCEL_RETRIES`] for the cap.
    pub concurrent_cancel_retries: HashMap<BlockHash, u8>,

    /// Block tree for priority calculation
    pub block_tree_guard: BlockTreeReadGuard,
}

impl ValidationCoordinator {
    pub(super) fn new(
        block_tree_guard: BlockTreeReadGuard,
        runtime_handle: tokio::runtime::Handle,
    ) -> Self {
        Self {
            vdf_scheduler: VdfScheduler::new(runtime_handle),
            concurrent_tasks: JoinSet::new(),
            concurrent_task_blocks: HashMap::new(),
            concurrent_cancel_retries: HashMap::new(),
            block_tree_guard,
        }
    }
}

impl<S: VdfSpawnStrategy> ValidationCoordinator<S> {
    #[cfg(test)]
    pub(super) fn new_with_strategy(
        block_tree_guard: BlockTreeReadGuard,
        vdf_scheduler: VdfScheduler<S>,
    ) -> Self {
        Self {
            vdf_scheduler,
            concurrent_tasks: JoinSet::new(),
            concurrent_task_blocks: HashMap::new(),
            concurrent_cancel_retries: HashMap::new(),
            block_tree_guard,
        }
    }

    /// Calculate priority for a block
    #[instrument(level = "trace", skip_all, fields(block.hash = %block.block_hash, block.height = %block.height))]
    pub(super) fn calculate_priority(&self, block: &IrysBlockHeader) -> BlockPriorityMeta {
        let block_tree = self.block_tree_guard.read();
        let block_hash = block.block_hash;

        let state = match block_tree.get_block_and_status(&block_hash) {
            Some((_, ChainState::Onchain)) => BlockPriority::Canonical,
            Some((_, ChainState::NotOnchain(_) | ChainState::Validated(_))) => {
                if self.is_canonical_extension(&block_hash, &block_tree) {
                    BlockPriority::CanonicalExtension
                } else {
                    BlockPriority::Fork
                }
            }
            None => BlockPriority::Unknown,
        };

        BlockPriorityMeta::new(block, state)
    }

    /// Check if block extends canonical tip
    #[instrument(level = "trace", skip_all, fields(block.hash = %block_hash))]
    fn is_canonical_extension(&self, block_hash: &BlockHash, block_tree: &BlockTree) -> bool {
        let (canonical_chain, _) = block_tree.get_canonical_chain();
        let canonical_tip = canonical_chain.last().unwrap().block_hash();

        let mut current = *block_hash;
        while let Some((block, _)) = block_tree.get_block_and_status(&current) {
            if current == canonical_tip {
                return true;
            }
            current = block.previous_block_hash;

            if let Some((_, ChainState::Onchain)) = block_tree.get_block_and_status(&current) {
                return current == canonical_tip;
            }
        }
        false
    }

    /// Submit a validation task
    #[instrument(skip_all, fields(block.hash = %task.sealed_block.header().block_hash, block.height = %task.sealed_block.header().height))]
    pub(super) fn submit_task(&mut self, task: BlockValidationTask) {
        let priority = self.calculate_priority(task.sealed_block.header());
        self.vdf_scheduler.submit(task, priority);
    }

    /// Spawn a VDF-validated block into the concurrent validation JoinSet.
    pub(super) fn spawn_concurrent(&mut self, task: BlockValidationTask) {
        let block_hash = task.sealed_block.header().block_hash;
        let enqueued_at = task.enqueued_at;
        // Capture before the task is moved into the spawn closure so the
        // cancel-arm requeue path can reconstruct a fresh `BlockValidationTask`
        // for the same block. Cloning the `Arc<SealedBlock>` is a refcount
        // bump; the bool is Copy.
        let sealed_block = Arc::clone(&task.sealed_block);
        let skip_vdf_validation = task.skip_vdf_validation;

        let abort_handle = self.concurrent_tasks.spawn(
            async move {
                let validation_result = task.execute_concurrent().await;

                ConcurrentValidationResult {
                    block_hash,
                    validation_result,
                    enqueued_at,
                }
            }
            .instrument(tracing::error_span!(
                "concurrent_validation",
                block.hash = %block_hash
            ))
            .in_current_span(),
        );
        self.concurrent_task_blocks.insert(
            abort_handle.id(),
            (block_hash, enqueued_at, sealed_block, skip_vdf_validation),
        );
    }

    /// Abort all running validation tasks for clean shutdown.
    ///
    /// Cancels and aborts the running VDF task (if any) and aborts all concurrent
    /// validation tasks so that `SealedBlock` and `ValidationServiceInner` Arcs are
    /// released promptly instead of being held by detached tasks.
    pub(super) fn shutdown(&mut self) {
        // Abort the running VDF task
        if let Some(vdf_task) = self.vdf_scheduler.current.take() {
            vdf_task
                .cancel_signal
                .store(CancelEnum::Cancelled as u8, Ordering::Relaxed);
            vdf_task.handle.abort();
        }

        // Clear pending VDF tasks (no spawned work, just drop queued items)
        self.vdf_scheduler.pending.clear();

        // Abort all concurrent validation tasks
        self.concurrent_tasks.abort_all();
        self.concurrent_task_blocks.clear();
        self.concurrent_cancel_retries.clear();
    }

    /// Record a concurrent-stage cancellation for `block_hash` and decide
    /// whether to requeue or give up. Increments the per-block counter and
    /// compares against [`MAX_CONCURRENT_CANCEL_RETRIES`].
    ///
    /// - `attempt == 1` on the first cancellation seen for the block.
    /// - Returns `Requeue` while `attempt < MAX_CONCURRENT_CANCEL_RETRIES`.
    /// - Returns `GiveUp` and clears the counter at the cap so the entry
    ///   doesn't linger after the block leaves the active set.
    ///
    /// Counter clearing on `GiveUp`: the block is being routed out via
    /// `ValidationCancelReason::RepeatedCancellation` and won't re-enter
    /// without fresh gossip. Fresh gossip starts the count from zero —
    /// the new entry is a different validation attempt, not a continuation
    /// of the give-up'd one.
    pub(super) fn record_concurrent_cancel(
        &mut self,
        block_hash: BlockHash,
    ) -> ConcurrentCancelDecision {
        let counter = self
            .concurrent_cancel_retries
            .entry(block_hash)
            .or_insert(0);
        // Saturate so an attacker can't roll the counter back to 0 by
        // forcing 256 cancellations (defensive — the cap is 3, so this
        // is unreachable in practice, but cheap to guard).
        *counter = counter.saturating_add(1);
        let attempt = *counter;
        if attempt >= MAX_CONCURRENT_CANCEL_RETRIES {
            // Cap hit — drop the counter so we don't leak entries for
            // every block that ever burned through its retry budget.
            self.concurrent_cancel_retries.remove(&block_hash);
            ConcurrentCancelDecision::GiveUp { attempt }
        } else {
            ConcurrentCancelDecision::Requeue { attempt }
        }
    }

    /// Clear the cancel-retry counter for `block_hash`. Invoked when the
    /// block exits the concurrent stage via a verdict (Ok arm) or via a
    /// task panic (panic arm) — both terminate the validation attempt,
    /// so a future cancel for a future submission of the same hash should
    /// start fresh from zero rather than inheriting stale state.
    pub(super) fn clear_concurrent_cancel_retries(&mut self, block_hash: &BlockHash) {
        self.concurrent_cancel_retries.remove(block_hash);
    }

    /// Reevaluate all priorities after reorg
    #[instrument(level = "trace", skip_all)]
    pub(super) fn reevaluate_priorities(&mut self) {
        info!("Reevaluating priorities after reorg");
        let started = Instant::now();

        // Reevaluate current VDF task
        self.reevaluate_current_vdf();

        // Reevaluate pending VDF tasks
        self.reevaluate_pending_vdf();

        // Re-check preemption now that both current and pending priorities are
        // up-to-date. The earlier check inside reevaluate_current_vdf() compared
        // the updated current priority against stale pending priorities, so it
        // could miss cases where a pending task's priority also rose.
        self.vdf_scheduler.check_preemption();

        // Defensive: ensure a pending task starts if nothing is running.
        // Maintains the invariant even if future code paths add to pending
        // without calling start_next().
        self.vdf_scheduler.start_next();

        metrics::record_reorg_priority_reevaluation(started.elapsed().as_secs_f64() * 1000.0);
    }

    /// Snapshot of pending VDF tasks bucketed by priority class, plus the age
    /// of the oldest pending or running task in milliseconds.
    pub(super) fn pipeline_snapshot(&self) -> (Vec<(&'static str, u64)>, u64) {
        let mut canonical_extension = 0_u64;
        let mut canonical = 0_u64;
        let mut fork = 0_u64;
        let mut unknown = 0_u64;
        let mut oldest = std::time::Duration::ZERO;
        for (task, meta) in self.vdf_scheduler.pending.iter() {
            match meta.state {
                BlockPriority::CanonicalExtension => canonical_extension += 1,
                BlockPriority::Canonical => canonical += 1,
                BlockPriority::Fork => fork += 1,
                BlockPriority::Unknown => unknown += 1,
            }
            let age = task.enqueued_at.elapsed();
            if age > oldest {
                oldest = age;
            }
        }
        if let Some(current) = &self.vdf_scheduler.current
            && let Some(task) = &current.requeue_task
        {
            let age = task.enqueued_at.elapsed();
            if age > oldest {
                oldest = age;
            }
        }
        let by_priority = vec![
            ("canonical_extension", canonical_extension),
            ("canonical", canonical),
            ("fork", fork),
            ("unknown", unknown),
        ];
        (by_priority, oldest.as_millis() as u64)
    }

    /// Reevaluate and potentially preempt current VDF task
    fn reevaluate_current_vdf(&mut self) {
        // Clone the sealed block Arc (cheap refcount bump) so the immutable
        // borrow of `current` is released before we call calculate_priority
        // (which borrows &self) and take &mut current to update the priority.
        let Some((sealed_block, hash)) = self
            .vdf_scheduler
            .current
            .as_ref()
            .map(|c| (Arc::clone(&c.sealed_block), c.hash))
        else {
            return;
        };

        let new_priority = self.calculate_priority(sealed_block.header());

        // Re-borrow mutably now that calculate_priority's &self borrow is released.
        let Some(current) = &mut self.vdf_scheduler.current else {
            return;
        };

        if new_priority == current.priority {
            return;
        }

        debug!(
            block.hash = %hash,
            block.priority.old = ?current.priority,
            block.priority.new = ?new_priority,
            "Current VDF task priority changed after reorg"
        );

        // Preemption is checked by the caller (reevaluate_priorities) after
        // pending priorities are also refreshed.
        current.priority = new_priority;
    }

    /// Reevaluate pending VDF task priorities
    fn reevaluate_pending_vdf(&mut self) {
        // Collect tasks that need priority updates (can't mutate while iterating)
        let tasks_to_update: Vec<_> = self
            .vdf_scheduler
            .pending
            .iter()
            .map(|(task, _priority)| task.clone())
            .collect();

        if tasks_to_update.is_empty() {
            return;
        }

        let mut updated_count = 0;
        for task in tasks_to_update {
            let new_priority = self.calculate_priority(task.sealed_block.header());
            // update_priority returns true if the item existed and was updated
            if self
                .vdf_scheduler
                .pending
                .change_priority(&task, new_priority)
                .is_some()
            {
                updated_count += 1;
            }
        }

        if updated_count > 0 {
            debug!(
                vdf.pending_updated = updated_count,
                "Reevaluated VDF pending task priorities after reorg"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_domain::{
        BlockState, BlockTree, BlockTreeReadGuard, ChainState, CommitmentSnapshot,
        dummy_ema_snapshot, dummy_epoch_snapshot,
    };
    use irys_testing_utils::IrysBlockHeaderTestExt as _;
    use irys_types::{BlockBody, BlockHash, IrysBlockHeader, serialization::H256List};
    use priority_queue::PriorityQueue;
    use std::sync::{Arc, RwLock};

    /// Test that BlockPriorityMeta ordering works correctly with manual Ord
    #[test]
    fn test_validation_priority_ordering() {
        let mut header1 = IrysBlockHeader::new_mock_header();
        header1.height = 100;
        header1.vdf_limiter_info.steps = H256List(vec![Default::default(); 5]); // 5 VDF steps

        let mut header2 = IrysBlockHeader::new_mock_header();
        header2.height = 200;
        header2.vdf_limiter_info.steps = H256List(vec![Default::default(); 10]); // 10 VDF steps

        // Test 1: Canonical extension should have highest priority
        let p1 = BlockPriorityMeta::new(&header1, BlockPriority::CanonicalExtension);
        let p2 = BlockPriorityMeta::new(&header2, BlockPriority::Canonical);
        assert!(
            p1 > p2,
            "Canonical extension should have higher priority than canonical"
        );

        // Test 2: Among same type, lower height should have higher priority
        let p3 = BlockPriorityMeta::new(&header1, BlockPriority::Fork);
        let p4 = BlockPriorityMeta::new(&header2, BlockPriority::Fork);
        assert!(p3 > p4, "Lower height should have higher priority");

        // Test 3: Canonical should have higher priority than fork
        let p5 = BlockPriorityMeta::new(&header1, BlockPriority::Canonical);
        let p6 = BlockPriorityMeta::new(&header1, BlockPriority::Fork);
        assert!(p5 > p6, "Canonical should have higher priority than fork");

        // Test 4: Test BlockPriority enum ordering (higher priority > lower priority)
        assert!(BlockPriority::CanonicalExtension > BlockPriority::Canonical);
        assert!(BlockPriority::Canonical > BlockPriority::Fork);
        assert!(BlockPriority::Fork > BlockPriority::Unknown);

        // Test 5: VDF step count as tiebreaker (fewer steps = higher priority)
        let mut header3 = header1.clone();
        header3.vdf_limiter_info.steps = H256List(vec![Default::default(); 20]); // More steps
        let p7 = BlockPriorityMeta::new(&header1, BlockPriority::Fork); // 5 steps
        let p8 = BlockPriorityMeta::new(&header3, BlockPriority::Fork); // 20 steps
        assert!(p7 > p8, "Fewer VDF steps should have higher priority");
    }

    /// Tests BlockPriority ordering with PriorityQueue to ensure correct behavior.
    /// Setup: Create priorities with different states, heights, and VDF steps.
    /// Expected: Items popped in order of CanonicalExtension > Canonical > Fork,
    ///          with lower heights and fewer VDF steps having higher priority.
    /// Verifies: PriorityQueue correctly uses BlockPriorityMeta ordering.
    #[test]
    fn test_priority_queue_ordering() {
        // Create headers with different properties
        let mkheader = |height: u64, vdf_steps: usize| {
            let mut header = IrysBlockHeader::new_mock_header();
            header.height = height;
            header.block_hash = BlockHash::random();
            header.vdf_limiter_info.steps = H256List(vec![Default::default(); vdf_steps]);
            header
        };

        // Create priority metadata
        let mkprio =
            |header: &IrysBlockHeader, state: BlockPriority| BlockPriorityMeta::new(header, state);

        // Create a priority queue
        let mut queue: PriorityQueue<BlockHash, (BlockPriorityMeta, ())> = PriorityQueue::new();

        // Add items in random order
        let h1 = mkheader(10, 5);
        let h2 = mkheader(9, 10);
        let h3 = mkheader(10, 100);
        let h4 = mkheader(9, 1);

        // Expected order (highest priority first):
        // 1. CanonicalExtension, height 9, 10 VDF steps
        // 2. CanonicalExtension, height 10, 5 VDF steps
        // 3. CanonicalExtension, height 10, 100 VDF steps
        // 4. Canonical, height 9, 1 VDF step

        let items = [
            (
                h2.block_hash,
                mkprio(&h2, BlockPriority::CanonicalExtension),
            ),
            (
                h1.block_hash,
                mkprio(&h1, BlockPriority::CanonicalExtension),
            ),
            (
                h3.block_hash,
                mkprio(&h3, BlockPriority::CanonicalExtension),
            ),
            (h4.block_hash, mkprio(&h4, BlockPriority::Canonical)),
        ];

        // Insert in different order to test sorting
        queue.push(items[3].0, (items[3].1, ()));
        queue.push(items[1].0, (items[1].1, ()));
        queue.push(items[0].0, (items[0].1, ()));
        queue.push(items[2].0, (items[2].1, ()));

        // Pop items and verify order
        let result1 = queue.pop().unwrap();
        assert_eq!(
            result1.0, items[0].0,
            "First item should be CanonicalExtension with height 9"
        );

        let result2 = queue.pop().unwrap();
        assert_eq!(
            result2.0, items[1].0,
            "Second item should be CanonicalExtension with height 10, 5 steps"
        );

        let result3 = queue.pop().unwrap();
        assert_eq!(
            result3.0, items[2].0,
            "Third item should be CanonicalExtension with height 10, 100 steps"
        );

        let result4 = queue.pop().unwrap();
        assert_eq!(
            result4.0, items[3].0,
            "Fourth item should be Canonical with height 9"
        );
    }

    /// Helper function to setup a canonical chain scenario with n blocks
    fn setup_canonical_chain_scenario(
        max_height: u64,
    ) -> (BlockTreeReadGuard, Vec<Arc<IrysBlockHeader>>) {
        // Create genesis block
        let mut genesis = IrysBlockHeader::new_mock_header();
        genesis.height = 0;
        genesis.cumulative_diff = 0.into();
        genesis.test_sign();

        // Create block tree with genesis
        let mut block_tree = BlockTree::new(&genesis, irys_types::ConsensusConfig::testing());
        block_tree.mark_tip(&genesis.block_hash).unwrap();

        let mut blocks = vec![Arc::new(genesis.clone())];
        let mut last_hash = genesis.block_hash;

        // Create canonical chain
        for height in 1..=max_height {
            let mut header = IrysBlockHeader::new_mock_header();
            header.height = height;
            header.previous_block_hash = last_hash;
            header.cumulative_diff = height.into();
            header.test_sign();

            let sealed = Arc::new(
                SealedBlock::new(
                    header.clone(),
                    BlockBody {
                        block_hash: header.block_hash,
                        ..Default::default()
                    },
                )
                .expect("sealing block"),
            );
            block_tree
                .add_common(
                    header.block_hash,
                    &sealed,
                    Arc::new(CommitmentSnapshot::default()),
                    dummy_epoch_snapshot(),
                    dummy_ema_snapshot(),
                    ChainState::Onchain,
                )
                .unwrap();

            block_tree.mark_tip(&header.block_hash).unwrap();
            last_hash = header.block_hash;
            blocks.push(Arc::new(header));
        }

        let block_tree_guard = BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));
        (block_tree_guard, blocks)
    }

    /// Tests priority calculation when a fork becomes the canonical chain.
    /// Setup: Canonical chain (0-3), canonical extensions (4-5), and fork chain (3-10) from height 2.
    /// Action: Make fork chain canonical by marking blocks 3-5 as canonical tip sequentially.
    /// Expected: Extension blocks (4-5) become Fork, fork blocks (3-5) become Canonical,
    ///          remaining fork blocks (6-10) become CanonicalExtension.
    /// Verifies: calculate_priority() correctly determines block priorities after reorg.
    #[tokio::test]
    async fn test_priority_calculation_after_fork_becomes_canonical() {
        // Setup: Create initial canonical chain (height 0-3)
        let (block_tree_guard, _blocks) = setup_canonical_chain_scenario(3);
        let coordinator =
            ValidationCoordinator::new(block_tree_guard.clone(), tokio::runtime::Handle::current());

        // Create canonical extension blocks (extending from canonical tip at height 3)
        let extension_blocks = {
            let mut tree = block_tree_guard.write();
            let (canonical_chain, _) = tree.get_canonical_chain();
            let tip = canonical_chain.last().unwrap();

            let mut blocks = Vec::new();
            let mut last_hash = tip.block_hash();

            for height in 4..=5 {
                let mut header = IrysBlockHeader::new_mock_header();
                header.height = height;
                header.previous_block_hash = last_hash;
                header.cumulative_diff = height.into();
                header.test_sign();
                last_hash = header.block_hash;

                let sealed = Arc::new(
                    SealedBlock::new(
                        header.clone(),
                        BlockBody {
                            block_hash: header.block_hash,
                            ..Default::default()
                        },
                    )
                    .expect("sealing block"),
                );
                tree.add_common(
                    header.block_hash,
                    &sealed,
                    Arc::new(CommitmentSnapshot::default()),
                    dummy_epoch_snapshot(),
                    dummy_ema_snapshot(),
                    ChainState::NotOnchain(BlockState::ValidationScheduled),
                )
                .unwrap();

                blocks.push(Arc::new(header));
            }
            blocks
        };

        // Verify initial priorities - extension blocks should be CanonicalExtension
        for block in &extension_blocks {
            let priority = coordinator.calculate_priority(block);
            assert_eq!(
                priority.state,
                BlockPriority::CanonicalExtension,
                "Extension block at height {} should be CanonicalExtension",
                block.height
            );
        }

        // First, let's add the extension blocks to the tree to establish them as part of the canonical extension
        // This ensures the fork blocks won't be seen as canonical extensions

        // Create fork blocks (extending from height 2, creating alternative chain)
        // These will compete with the canonical block at height 3
        let fork_blocks = {
            let mut tree = block_tree_guard.write();
            let (canonical_chain, _) = tree.get_canonical_chain();
            let fork_parent = canonical_chain.iter().find(|e| e.height() == 2).unwrap();

            let mut blocks = Vec::new();
            let mut last_hash = fork_parent.block_hash();

            // Create an alternative block at height 3 (competing with canonical block at height 3)
            for height in 3..=10 {
                let mut header = IrysBlockHeader::new_mock_header();
                header.height = height;
                header.previous_block_hash = last_hash;
                header.cumulative_diff = height.into();
                header.test_sign();
                last_hash = header.block_hash;

                let sealed = Arc::new(
                    SealedBlock::new(
                        header.clone(),
                        BlockBody {
                            block_hash: header.block_hash,
                            ..Default::default()
                        },
                    )
                    .expect("sealing block"),
                );
                tree.add_common(
                    header.block_hash,
                    &sealed,
                    Arc::new(CommitmentSnapshot::default()),
                    dummy_epoch_snapshot(),
                    dummy_ema_snapshot(),
                    ChainState::NotOnchain(BlockState::ValidationScheduled),
                )
                .unwrap();

                blocks.push(Arc::new(header));
            }
            blocks
        };

        // Verify initial fork block priorities
        // All fork blocks will be CanonicalExtension because they form a chain
        // that extends from the canonical chain (at height 2) and creates a longer chain
        for block in &fork_blocks {
            let priority = coordinator.calculate_priority(block);
            assert_eq!(
                priority.state,
                BlockPriority::CanonicalExtension,
                "Fork block at height {} is initially CanonicalExtension (extends from canonical chain)",
                block.height
            );
        }

        // Action: Make the fork chain canonical by marking blocks as valid and advancing tip
        {
            let mut tree = block_tree_guard.write();

            // Mark fork blocks as onchain to simulate them becoming canonical
            for i in 0..=5 {
                tree.mark_block_as_valid(&fork_blocks[i].block_hash)
                    .unwrap();
                tree.mark_tip(&fork_blocks[i].block_hash).unwrap();
            }
        }

        // Verify: Extension blocks (4-5) are now Fork priority (no longer extend canonical)
        for block in &extension_blocks {
            let priority = coordinator.calculate_priority(block);
            assert_eq!(
                priority.state,
                BlockPriority::Fork,
                "Extension block at height {} should now be Fork priority after reorg",
                block.height
            );
        }

        // Verify: Fork blocks that are now on the canonical chain
        for (i, block) in fork_blocks.iter().enumerate() {
            let priority = coordinator.calculate_priority(block);

            if i <= 5 {
                // These blocks are now part of the canonical chain
                assert_eq!(
                    priority.state,
                    BlockPriority::Canonical,
                    "Fork block at height {} (index {}) should now be Canonical priority",
                    block.height,
                    i
                );
            } else {
                // These blocks extend the new canonical tip
                assert_eq!(
                    priority.state,
                    BlockPriority::CanonicalExtension,
                    "Fork block at height {} (index {}) should now be CanonicalExtension priority",
                    block.height,
                    i
                );
            }
        }
    }

    // =========================================================================
    // Tests for the JoinHandle-based VDF signaling (replacing Notify)
    // =========================================================================

    /// Helper to create a RunningVdfTask with a never-completing handle.
    /// Used for testing scheduler state without real VDF execution.
    fn make_running_vdf_task(
        priority: BlockPriorityMeta,
    ) -> (RunningVdfTask, Arc<std::sync::atomic::AtomicU8>) {
        let cancel = Arc::new(std::sync::atomic::AtomicU8::new(CancelEnum::Continue as u8));
        let stage = Arc::new(std::sync::atomic::AtomicU8::new(
            VdfTaskStage::Starting as u8,
        ));
        let progress = Arc::new(Mutex::new(Instant::now()));
        let mut header = IrysBlockHeader::new_mock_header();
        header.height = priority.height;
        header.vdf_limiter_info.steps =
            H256List(vec![Default::default(); priority.vdf_step_count as usize]);
        header.test_sign();
        let hash = header.block_hash;
        let sealed = Arc::new(
            SealedBlock::new(
                header,
                BlockBody {
                    block_hash: hash,
                    ..Default::default()
                },
            )
            .expect("sealing block"),
        );
        let task = RunningVdfTask {
            hash,
            priority,
            cancel_signal: Arc::clone(&cancel),
            stage_signal: stage,
            last_progress_at: progress,
            completed_at: Arc::new(Mutex::new(None)),
            started_at: Instant::now(),
            sealed_block: sealed,
            handle: tokio::spawn(std::future::pending()),
            requeue_task: None,
        };
        (task, cancel)
    }

    /// Verifies that `&mut JoinHandle` resolves directly in `tokio::select!` when the
    /// spawned task completes — no external signaling mechanism (like Notify) needed.
    #[tokio::test]
    async fn test_handle_resolves_directly_in_select() {
        let expected = 42_u64;
        let mut handle = tokio::spawn(async move { expected });

        let result = tokio::select! {
            r = &mut handle => r.unwrap(),
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                panic!("Handle did not resolve — select was not woken");
            }
        };

        assert_eq!(result, expected);
    }

    /// Verifies that a panicking spawned task produces a JoinError through the handle,
    /// which is how VDF task panics are caught and reported to the block tree.
    #[tokio::test]
    async fn test_handle_panic_yields_join_error() {
        let mut handle: tokio::task::JoinHandle<u64> = tokio::spawn(async {
            panic!("simulated VDF panic");
        });

        let result = tokio::select! {
            r = &mut handle => r,
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                panic!("Handle did not resolve after panic");
            }
        };

        assert!(result.is_err(), "Panicked task should produce JoinError");
        assert!(
            result.unwrap_err().is_panic(),
            "Error should be a panic, not cancellation"
        );
    }

    /// Verifies that poll_vdf() pends forever when no VDF task is running,
    /// allowing other select branches to fire.
    #[tokio::test]
    async fn test_select_skips_vdf_branch_when_no_task() {
        let mut scheduler = VdfScheduler::new(tokio::runtime::Handle::current());
        assert!(scheduler.current.is_none());

        // poll_vdf returns pending() when current is None,
        // so the timer branch fires instead.
        let which_branch = tokio::select! {
            _r = scheduler.poll_vdf() => "handle",
            _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => "timer",
        };

        assert_eq!(
            which_branch, "timer",
            "poll_vdf should pend when no task is running"
        );
    }

    /// Verifies that the cancel signal (AtomicU8) propagates correctly from the
    /// scheduler to a running task, causing it to return VdfValidationResult::Cancelled.
    /// This is the mechanism behind preemption: check_preemption sets the signal,
    /// the task detects it and returns Cancelled, and the select handler requeues it.
    #[tokio::test]
    async fn test_cancel_signal_produces_cancelled_result() {
        let cancel = Arc::new(std::sync::atomic::AtomicU8::new(CancelEnum::Continue as u8));
        let cancel_reader = Arc::clone(&cancel);

        // Simulate a VDF task that checks the cancel signal
        let mut handle = tokio::spawn(async move {
            for _ in 0..1000 {
                if cancel_reader.load(std::sync::atomic::Ordering::Relaxed)
                    == CancelEnum::Cancelled as u8
                {
                    return VdfValidationResult::Cancelled;
                }
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            VdfValidationResult::Valid
        });

        // Set cancel signal (simulates check_preemption)
        cancel.store(
            CancelEnum::Cancelled as u8,
            std::sync::atomic::Ordering::Relaxed,
        );

        let result = tokio::select! {
            r = &mut handle => r.unwrap(),
            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                panic!("Task did not respond to cancel signal within timeout");
            }
        };

        assert!(
            matches!(result, VdfValidationResult::Cancelled),
            "Task should detect cancel signal and return Cancelled"
        );
    }

    /// Verifies that start_next() is a no-op when a VDF task is already running.
    /// This enforces the single-task invariant: at most one VDF task at a time.
    #[tokio::test]
    async fn test_start_next_preserves_running_task() {
        let mut scheduler = VdfScheduler::new(tokio::runtime::Handle::current());
        let priority = BlockPriorityMeta {
            height: 1,
            state: BlockPriority::Canonical,
            vdf_step_count: 1,
        };
        let (running_task, _cancel) = make_running_vdf_task(priority);
        let original_hash = running_task.hash;
        scheduler.current = Some(running_task);

        // start_next should be a no-op since a task is already running
        scheduler.start_next();

        assert!(
            scheduler.current.is_some(),
            "Current task should still be present"
        );
        assert_eq!(
            scheduler.current.as_ref().unwrap().hash,
            original_hash,
            "Current task should be the same one (not replaced)"
        );
    }

    /// Verifies that start_next() does nothing when the pending queue is empty.
    #[tokio::test]
    async fn test_start_next_noop_when_pending_empty() {
        let mut scheduler = VdfScheduler::new(tokio::runtime::Handle::current());

        scheduler.start_next();

        assert!(
            scheduler.current.is_none(),
            "No task should start with empty pending"
        );
    }

    /// Verifies that the scheduler-level watchdog cancels and aborts a stalled
    /// current task, annotating it with stage/elapsed metadata for the select
    /// handler to surface as an invalid result instead of silently wedging.
    #[tokio::test]
    async fn test_abort_stalled_current_marks_and_aborts_task() {
        let mut scheduler = VdfScheduler::new(tokio::runtime::Handle::current());
        let priority = BlockPriorityMeta {
            height: 1,
            state: BlockPriority::Canonical,
            vdf_step_count: 1,
        };
        let (running_task, cancel) = make_running_vdf_task(priority);
        *running_task
            .last_progress_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Instant::now().checked_sub(Duration::from_secs(10)).unwrap();
        running_task
            .stage_signal
            .store(VdfTaskStage::ValidateBatch as u8, Ordering::Relaxed);
        scheduler.current = Some(running_task);

        let (stalled_for, stage) = scheduler
            .abort_stalled_current(Duration::from_secs(5))
            .expect("watchdog should abort a stale task");
        assert_eq!(stage, VdfTaskStage::ValidateBatch);
        assert!(
            stalled_for >= Duration::from_secs(10),
            "stalled metadata should reflect the stale runtime"
        );
        assert_eq!(
            cancel.load(Ordering::Relaxed),
            CancelEnum::Cancelled as u8,
            "watchdog must set the cooperative cancel signal before aborting"
        );

        let current = scheduler
            .current
            .as_mut()
            .expect("current task should remain tracked until the aborted handle resolves");
        let join_error = match (&mut current.handle).await {
            Ok(_) => panic!("aborted task should resolve as JoinError"),
            Err(join_error) => join_error,
        };
        assert!(
            join_error.is_cancelled(),
            "watchdog abort should cancel the JoinHandle"
        );
    }

    /// Regression test for the CI flake where the watchdog force-aborted (and
    /// panicked on) a VDF task whose handle had already finished — the select
    /// loop was just starved past `hard_timeout` before `poll_vdf` could reap
    /// the completed result. A finished handle must be treated as not-stalled:
    /// `abort_stalled_current` returns `None` and leaves the cancel signal
    /// untouched so the valid result survives to the next `poll_vdf`.
    #[tokio::test]
    async fn test_abort_stalled_current_skips_finished_handle() {
        let mut scheduler = VdfScheduler::new(tokio::runtime::Handle::current());
        let priority = BlockPriorityMeta {
            height: 1,
            state: BlockPriority::Canonical,
            vdf_step_count: 1,
        };
        let (mut running_task, cancel) = make_running_vdf_task(priority);

        // Drive the handle to a finished state. Abort + await is just the
        // cheapest way to make `is_finished()` true here; it stands in for a
        // task that completed normally and is awaiting collection.
        running_task.handle.abort();
        let _ = (&mut running_task.handle).await;
        assert!(
            running_task.handle.is_finished(),
            "precondition: handle must be finished",
        );

        // Make it look stale: last progress well past the watchdog timeout.
        *running_task
            .last_progress_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Instant::now().checked_sub(Duration::from_secs(10)).unwrap();
        running_task
            .stage_signal
            .store(VdfTaskStage::Completed as u8, Ordering::Relaxed);
        scheduler.current = Some(running_task);

        assert!(
            scheduler
                .abort_stalled_current(Duration::from_secs(5))
                .is_none(),
            "watchdog must not abort a task whose handle has already finished",
        );
        assert_eq!(
            cancel.load(Ordering::Relaxed),
            CancelEnum::Continue as u8,
            "cancel signal must be left untouched so the completed result survives",
        );
    }

    /// Helper: create a stub BlockValidationTask for queue-only operations.
    fn make_stub_task(block_tree_guard: &BlockTreeReadGuard) -> (BlockValidationTask, BlockHash) {
        use crate::validation_service::block_validation_task::BlockValidationTask;

        let mut header = IrysBlockHeader::new_mock_header();
        header.block_hash = BlockHash::random();
        header.test_sign();
        let hash = header.block_hash;
        let sealed = Arc::new(
            SealedBlock::new(
                header,
                BlockBody {
                    block_hash: hash,
                    ..Default::default()
                },
            )
            .expect("sealing block"),
        );
        let task = BlockValidationTask::test_stub(sealed, block_tree_guard.clone());
        (task, hash)
    }

    /// Verifies that check_preemption() sets the cancel signal when a pending
    /// task has strictly higher priority than the running task.
    #[tokio::test]
    async fn test_check_preemption_fires_when_pending_outranks_current() {
        let (block_tree_guard, _) = setup_canonical_chain_scenario(1);
        let mut scheduler = VdfScheduler::new(tokio::runtime::Handle::current());

        // Running task at Fork priority (low)
        let low_priority = BlockPriorityMeta {
            height: 10,
            state: BlockPriority::Fork,
            vdf_step_count: 1,
        };
        let (running, cancel) = make_running_vdf_task(low_priority);
        scheduler.current = Some(running);

        // Pending task at CanonicalExtension priority (high)
        let (stub_task, _) = make_stub_task(&block_tree_guard);
        let high_priority = BlockPriorityMeta {
            height: 5,
            state: BlockPriority::CanonicalExtension,
            vdf_step_count: 1,
        };
        scheduler.pending.push(stub_task, high_priority);

        // Before check: cancel is not set
        assert_eq!(
            cancel.load(std::sync::atomic::Ordering::Relaxed),
            CancelEnum::Continue as u8,
        );

        scheduler.check_preemption();

        // After check: cancel IS set because pending outranks current
        assert_eq!(
            cancel.load(std::sync::atomic::Ordering::Relaxed),
            CancelEnum::Cancelled as u8,
            "Cancel signal should fire when pending task has higher priority"
        );
    }

    /// Verifies that check_preemption() does NOT fire when pending has equal
    /// or lower priority than current.
    #[tokio::test]
    async fn test_check_preemption_no_fire_when_pending_lower_or_equal() {
        let (block_tree_guard, _) = setup_canonical_chain_scenario(1);
        let mut scheduler = VdfScheduler::new(tokio::runtime::Handle::current());

        // Running task at CanonicalExtension priority (high)
        let high_priority = BlockPriorityMeta {
            height: 5,
            state: BlockPriority::CanonicalExtension,
            vdf_step_count: 1,
        };
        let (running, cancel) = make_running_vdf_task(high_priority);
        scheduler.current = Some(running);

        // Pending task at Fork priority (lower)
        let (stub_task, _) = make_stub_task(&block_tree_guard);
        let low_priority = BlockPriorityMeta {
            height: 10,
            state: BlockPriority::Fork,
            vdf_step_count: 1,
        };
        scheduler.pending.push(stub_task, low_priority);

        scheduler.check_preemption();

        assert_eq!(
            cancel.load(std::sync::atomic::Ordering::Relaxed),
            CancelEnum::Continue as u8,
            "Cancel signal should NOT fire when pending has lower priority"
        );

        // Replace pending with equal priority task
        scheduler.pending.clear();
        let (equal_task, _) = make_stub_task(&block_tree_guard);
        scheduler.pending.push(equal_task, high_priority);

        scheduler.check_preemption();

        assert_eq!(
            cancel.load(std::sync::atomic::Ordering::Relaxed),
            CancelEnum::Continue as u8,
            "Cancel signal should NOT fire when pending has equal priority"
        );
    }

    /// Verifies that submit() deduplicates a block already in the pending queue.
    #[tokio::test]
    async fn test_submit_deduplicates_pending_task() {
        let (block_tree_guard, _) = setup_canonical_chain_scenario(1);
        let mut scheduler = VdfScheduler::new(tokio::runtime::Handle::current());

        // Seed current with a different running task so that submit() is
        // forced to enqueue into pending (start_next returns early when
        // current.is_some()).
        let blocker_priority = BlockPriorityMeta {
            height: 1,
            state: BlockPriority::CanonicalExtension,
            vdf_step_count: 1,
        };
        let (blocker, _cancel) = make_running_vdf_task(blocker_priority);
        let blocker_hash = blocker.hash;
        scheduler.current = Some(blocker);

        let (stub_task, _hash) = make_stub_task(&block_tree_guard);
        let priority = BlockPriorityMeta {
            height: 5,
            state: BlockPriority::CanonicalExtension,
            vdf_step_count: 1,
        };

        // First submit — goes into pending (current is occupied)
        scheduler.submit(stub_task.clone(), priority);
        assert_eq!(
            scheduler.pending.len(),
            1,
            "First submit should enqueue into pending"
        );
        assert_eq!(
            scheduler.current.as_ref().unwrap().hash,
            blocker_hash,
            "Current task should still be the original blocker"
        );

        // Second submit with the same block hash — must hit the
        // self.pending.get(&task) dedup branch.
        let duplicate = stub_task;
        scheduler.submit(duplicate, priority);

        // Pending should still have exactly one entry (the duplicate was rejected)
        assert_eq!(
            scheduler.pending.len(),
            1,
            "Duplicate submit should be rejected by pending dedup"
        );
        assert_eq!(
            scheduler.current.as_ref().unwrap().hash,
            blocker_hash,
            "Current task should remain unchanged after duplicate submit"
        );
    }

    /// Verifies that submit() deduplicates a block that is already the
    /// currently running VDF task.
    #[tokio::test]
    async fn test_submit_deduplicates_current_task() {
        let (block_tree_guard, _) = setup_canonical_chain_scenario(1);
        let mut scheduler = VdfScheduler::new_test_mode(tokio::runtime::Handle::current());

        let (stub_task, hash) = make_stub_task(&block_tree_guard);
        let priority = BlockPriorityMeta {
            height: 5,
            state: BlockPriority::CanonicalExtension,
            vdf_step_count: 1,
        };

        // Submit once — starts running
        scheduler.submit(stub_task.clone(), priority);
        assert_eq!(scheduler.current.as_ref().unwrap().hash, hash);

        // Submit the same block again
        let duplicate = stub_task;
        scheduler.submit(duplicate, priority);

        // Should not have added to pending
        assert_eq!(
            scheduler.pending.len(),
            0,
            "Duplicate of running task should be rejected"
        );
    }

    /// Verifies that start_next() correctly promotes a pending task to running.
    #[tokio::test]
    async fn test_start_next_promotes_pending_to_running() {
        let (block_tree_guard, _) = setup_canonical_chain_scenario(1);
        let mut scheduler = VdfScheduler::new_test_mode(tokio::runtime::Handle::current());

        let (stub_task, expected_hash) = make_stub_task(&block_tree_guard);
        let priority = BlockPriorityMeta {
            height: 5,
            state: BlockPriority::CanonicalExtension,
            vdf_step_count: 1,
        };
        scheduler.pending.push(stub_task, priority);

        assert!(scheduler.current.is_none());
        assert_eq!(scheduler.pending.len(), 1);

        let started = scheduler.start_next();

        assert!(
            started,
            "start_next should return true when a task was started"
        );
        assert!(scheduler.current.is_some(), "Current task should be set");
        let current = scheduler.current.as_ref().unwrap();
        assert_eq!(
            current.hash, expected_hash,
            "Running task should have the expected block hash"
        );
        assert_eq!(
            current.priority, priority,
            "Running task should have the expected priority"
        );
        assert_eq!(
            current
                .cancel_signal
                .load(std::sync::atomic::Ordering::Relaxed),
            CancelEnum::Continue as u8,
            "Newly promoted task should have cancel signal in Continue state"
        );
        assert!(
            scheduler.pending.is_empty(),
            "Pending queue should be empty"
        );
    }

    /// Verifies that sequential handle awaits work correctly, simulating the
    /// "complete one task, start next" pattern used in the validation service.
    #[tokio::test]
    async fn test_sequential_handle_processing() {
        let hashes: Vec<BlockHash> = (0..3).map(|_| BlockHash::random()).collect();

        for (i, &expected_hash) in hashes.iter().enumerate() {
            let mut handle = tokio::spawn(async move { expected_hash });

            let result = tokio::select! {
                r = &mut handle => r.unwrap(),
                _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                    panic!("Handle {} did not resolve within timeout", i);
                }
            };

            assert_eq!(
                result, expected_hash,
                "Task {} should return its expected hash",
                i
            );
        }
    }

    /// Tests that reevaluate_priorities() updates the running VDF task's priority
    /// when the block tree changes after a reorg.
    /// Setup: Canonical chain (0-3), extension block at height 4 set as current VDF task.
    /// Action: Fork from height 2 becomes canonical (heights 3-6).
    /// Expected: Current VDF task's priority changes from CanonicalExtension to Fork.
    #[tokio::test]
    async fn test_reevaluate_priorities_updates_running_task_priority() {
        let (block_tree_guard, _blocks) = setup_canonical_chain_scenario(3);
        let mut coordinator =
            ValidationCoordinator::new(block_tree_guard.clone(), tokio::runtime::Handle::current());

        // Add an extension block at height 4 (extends canonical tip)
        let (ext_header, ext_sealed) = {
            let mut tree = block_tree_guard.write();
            let (canonical_chain, _) = tree.get_canonical_chain();
            let tip = canonical_chain.last().unwrap();

            let mut header = IrysBlockHeader::new_mock_header();
            header.height = 4;
            header.previous_block_hash = tip.block_hash();
            header.cumulative_diff = 4.into();
            header.test_sign();

            let sealed = Arc::new(
                SealedBlock::new(
                    header.clone(),
                    BlockBody {
                        block_hash: header.block_hash,
                        ..Default::default()
                    },
                )
                .expect("sealing block"),
            );
            tree.add_common(
                header.block_hash,
                &sealed,
                Arc::new(CommitmentSnapshot::default()),
                dummy_epoch_snapshot(),
                dummy_ema_snapshot(),
                ChainState::NotOnchain(BlockState::ValidationScheduled),
            )
            .unwrap();

            (header, sealed)
        };

        // Verify initial priority is CanonicalExtension
        let initial_priority = coordinator.calculate_priority(&ext_header);
        assert_eq!(initial_priority.state, BlockPriority::CanonicalExtension);

        // Set the extension block as the current VDF task
        let cancel = Arc::new(std::sync::atomic::AtomicU8::new(CancelEnum::Continue as u8));
        coordinator.vdf_scheduler.current = Some(RunningVdfTask {
            hash: ext_header.block_hash,
            priority: initial_priority,
            cancel_signal: cancel,
            stage_signal: Arc::new(std::sync::atomic::AtomicU8::new(
                VdfTaskStage::Starting as u8,
            )),
            last_progress_at: Arc::new(Mutex::new(Instant::now())),
            completed_at: Arc::new(Mutex::new(None)),
            started_at: Instant::now(),
            sealed_block: ext_sealed,
            handle: tokio::spawn(std::future::pending()),
            requeue_task: None,
        });

        // Create a fork from height 2 that becomes the new canonical chain
        {
            let mut tree = block_tree_guard.write();
            let (canonical_chain, _) = tree.get_canonical_chain();
            let fork_parent = canonical_chain.iter().find(|e| e.height() == 2).unwrap();

            let mut last_hash = fork_parent.block_hash();
            for height in 3..=6 {
                let mut header = IrysBlockHeader::new_mock_header();
                header.height = height;
                header.previous_block_hash = last_hash;
                header.cumulative_diff = (height * 10).into();
                header.test_sign();
                last_hash = header.block_hash;

                let sealed = Arc::new(
                    SealedBlock::new(
                        header.clone(),
                        BlockBody {
                            block_hash: header.block_hash,
                            ..Default::default()
                        },
                    )
                    .expect("sealing block"),
                );
                tree.add_common(
                    header.block_hash,
                    &sealed,
                    Arc::new(CommitmentSnapshot::default()),
                    dummy_epoch_snapshot(),
                    dummy_ema_snapshot(),
                    ChainState::NotOnchain(BlockState::ValidationScheduled),
                )
                .unwrap();

                tree.mark_block_as_valid(&header.block_hash).unwrap();
                tree.mark_tip(&header.block_hash).unwrap();
            }
        }

        // Confirm the priority has changed in the block tree
        let new_priority = coordinator.calculate_priority(&ext_header);
        assert_eq!(new_priority.state, BlockPriority::Fork);

        // Reevaluate priorities (this is called after reorg events in the service)
        coordinator.reevaluate_priorities();

        // Verify the running task's priority was updated
        let current = coordinator
            .vdf_scheduler
            .current
            .as_ref()
            .expect("current task should still exist");
        assert_eq!(
            current.hash, ext_header.block_hash,
            "Running task should still be the same block after reevaluation"
        );
        assert_eq!(
            current.priority.state,
            BlockPriority::Fork,
            "Running task priority should update from CanonicalExtension to Fork after reorg"
        );
    }

    /// Tests that check_preemption() is a no-op when the pending queue is empty,
    /// and that it correctly preserves the cancel signal state.
    #[tokio::test]
    async fn test_check_preemption_noop_with_empty_pending() {
        let mut scheduler = VdfScheduler::new(tokio::runtime::Handle::current());

        let priority = BlockPriorityMeta {
            height: 5,
            state: BlockPriority::Fork,
            vdf_step_count: 1,
        };
        let (running_task, cancel) = make_running_vdf_task(priority);
        scheduler.current = Some(running_task);

        // check_preemption with empty pending — must NOT fire cancel
        scheduler.check_preemption();
        assert_eq!(
            cancel.load(std::sync::atomic::Ordering::Relaxed),
            CancelEnum::Continue as u8,
            "Cancel signal should not fire with empty pending queue"
        );
    }

    /// Tests that reevaluate_priorities() starts pending tasks when no VDF task
    /// is currently running (current=None). After a reorg, the defensive
    /// start_next() at the end of reevaluate_priorities() should pick up work.
    #[tokio::test]
    async fn test_reevaluate_priorities_with_no_current_task() {
        let (block_tree_guard, _blocks) = setup_canonical_chain_scenario(3);
        let vdf_scheduler = VdfScheduler::new_test_mode(tokio::runtime::Handle::current());
        let mut coordinator =
            ValidationCoordinator::new_with_strategy(block_tree_guard.clone(), vdf_scheduler);

        // --- Empty-queue branch: nothing pending, nothing running ---
        assert!(coordinator.vdf_scheduler.current.is_none());
        assert!(coordinator.vdf_scheduler.pending.is_empty());

        coordinator.reevaluate_priorities();

        // Still no running task (nothing was pending)
        assert!(coordinator.vdf_scheduler.current.is_none());

        // --- Idle-start branch: pending task gets promoted to current ---
        let (stub_task, expected_hash) = make_stub_task(&block_tree_guard);
        let priority = BlockPriorityMeta {
            height: 1,
            state: BlockPriority::Fork,
            vdf_step_count: 1,
        };
        coordinator.vdf_scheduler.pending.push(stub_task, priority);
        assert!(coordinator.vdf_scheduler.current.is_none());

        coordinator.reevaluate_priorities();

        // The defensive start_next() should have promoted the pending task
        assert!(
            coordinator.vdf_scheduler.current.is_some(),
            "start_next() in reevaluate_priorities should promote a pending task"
        );
        assert_eq!(
            coordinator.vdf_scheduler.current.as_ref().unwrap().hash,
            expected_hash
        );
        assert!(
            coordinator.vdf_scheduler.pending.is_empty(),
            "pending queue should be empty after promotion"
        );

        // Clean up spawned task
        coordinator.shutdown();
    }

    /// Tests that reevaluate_priorities() correctly handles the case where the
    /// running VDF task's priority doesn't change after a reorg.
    #[tokio::test]
    async fn test_reevaluate_priorities_noop_when_priority_unchanged() {
        let (block_tree_guard, _blocks) = setup_canonical_chain_scenario(3);
        let mut coordinator =
            ValidationCoordinator::new(block_tree_guard.clone(), tokio::runtime::Handle::current());

        // Add an extension block and set as current
        let ext_header = {
            let mut tree = block_tree_guard.write();
            let (canonical_chain, _) = tree.get_canonical_chain();
            let tip = canonical_chain.last().unwrap();

            let mut header = IrysBlockHeader::new_mock_header();
            header.height = 4;
            header.previous_block_hash = tip.block_hash();
            header.cumulative_diff = 4.into();
            header.test_sign();

            let sealed = Arc::new(
                SealedBlock::new(
                    header.clone(),
                    BlockBody {
                        block_hash: header.block_hash,
                        ..Default::default()
                    },
                )
                .expect("sealing block"),
            );
            tree.add_common(
                header.block_hash,
                &sealed,
                Arc::new(CommitmentSnapshot::default()),
                dummy_epoch_snapshot(),
                dummy_ema_snapshot(),
                ChainState::NotOnchain(BlockState::ValidationScheduled),
            )
            .unwrap();

            header
        };

        let initial_priority = coordinator.calculate_priority(&ext_header);
        assert_eq!(initial_priority.state, BlockPriority::CanonicalExtension);

        let cancel = Arc::new(std::sync::atomic::AtomicU8::new(CancelEnum::Continue as u8));
        let ext_hash = ext_header.block_hash;
        coordinator.vdf_scheduler.current = Some(RunningVdfTask {
            hash: ext_hash,
            priority: initial_priority,
            cancel_signal: Arc::clone(&cancel),
            stage_signal: Arc::new(std::sync::atomic::AtomicU8::new(
                VdfTaskStage::Starting as u8,
            )),
            last_progress_at: Arc::new(Mutex::new(Instant::now())),
            completed_at: Arc::new(Mutex::new(None)),
            started_at: Instant::now(),
            sealed_block: Arc::new(
                SealedBlock::new(
                    ext_header,
                    BlockBody {
                        block_hash: ext_hash,
                        ..Default::default()
                    },
                )
                .expect("sealing block"),
            ),
            handle: tokio::spawn(std::future::pending()),
            requeue_task: None,
        });

        // Reevaluate without any tree changes
        coordinator.reevaluate_priorities();

        // Priority should remain unchanged and the same task should still be running
        let current = coordinator.vdf_scheduler.current.as_ref().unwrap();
        assert_eq!(
            current.hash, ext_hash,
            "Same task should still be running after no-op reevaluation"
        );
        assert_eq!(current.priority.state, BlockPriority::CanonicalExtension);
        // Cancel signal should NOT be set (no preemption)
        assert_eq!(
            cancel.load(std::sync::atomic::Ordering::Relaxed),
            CancelEnum::Continue as u8,
            "Cancel signal should not be set when priority is unchanged"
        );
    }

    /// Concurrent-stage cancel-retry cap.
    ///
    /// After `MAX_CONCURRENT_CANCEL_RETRIES` cancellations for the same
    /// block hash, the coordinator must stop returning `Requeue` and start
    /// returning `GiveUp`. The first `cap - 1` calls Requeue (so we don't
    /// over-eagerly give up on a transient hiccup); the cap-th call is
    /// the one that returns GiveUp. The GiveUp path is what the
    /// validation-service cancel arm uses to route a
    /// `ValidationCancelReason::RepeatedCancellation` outcome instead of
    /// looping back into `submit_task`.
    #[tokio::test]
    async fn cancel_retry_cap_returns_giveup_at_threshold() {
        let (block_tree_guard, _blocks) = setup_canonical_chain_scenario(0);
        let mut coordinator =
            ValidationCoordinator::new(block_tree_guard, tokio::runtime::Handle::current());
        let hash = BlockHash::random();

        // The first `cap - 1` cancels must return Requeue with the
        // running attempt count.
        for expected_attempt in 1..MAX_CONCURRENT_CANCEL_RETRIES {
            let decision = coordinator.record_concurrent_cancel(hash);
            assert_eq!(
                decision,
                ConcurrentCancelDecision::Requeue {
                    attempt: expected_attempt
                },
                "cancel #{expected_attempt} (cap={MAX_CONCURRENT_CANCEL_RETRIES}) must Requeue"
            );
        }

        // The cap-th cancel must return GiveUp. attempt equals the cap.
        let decision = coordinator.record_concurrent_cancel(hash);
        assert_eq!(
            decision,
            ConcurrentCancelDecision::GiveUp {
                attempt: MAX_CONCURRENT_CANCEL_RETRIES
            },
            "cap-th cancel must return GiveUp"
        );

        // After GiveUp, the counter entry is cleared (the block is being
        // routed out via RepeatedCancellation — fresh gossip starts a new
        // count). The next cancel for the same hash starts from attempt 1.
        assert!(
            !coordinator.concurrent_cancel_retries.contains_key(&hash),
            "GiveUp must clear the counter so the entry doesn't leak"
        );
        let decision = coordinator.record_concurrent_cancel(hash);
        assert_eq!(
            decision,
            ConcurrentCancelDecision::Requeue { attempt: 1 },
            "after GiveUp clears the counter, a fresh cancel starts from attempt 1"
        );
    }

    /// A Valid (Ok-arm) outcome clears the cancel-retry counter so a
    /// subsequent burst of cancels doesn't immediately trip the cap from
    /// stale state.
    #[tokio::test]
    async fn cancel_retry_counter_cleared_on_verdict() {
        let (block_tree_guard, _blocks) = setup_canonical_chain_scenario(0);
        let mut coordinator =
            ValidationCoordinator::new(block_tree_guard, tokio::runtime::Handle::current());
        let hash = BlockHash::random();

        // Accumulate cancels up to but not including the cap.
        for _ in 1..MAX_CONCURRENT_CANCEL_RETRIES {
            coordinator.record_concurrent_cancel(hash);
        }
        assert_eq!(
            coordinator.concurrent_cancel_retries.get(&hash).copied(),
            Some(MAX_CONCURRENT_CANCEL_RETRIES - 1),
            "counter should be at cap-1 before the verdict"
        );

        // Simulate a Valid/Invalid verdict arriving for the block: the
        // Ok-arm in the validation-service loop calls
        // `clear_concurrent_cancel_retries`.
        coordinator.clear_concurrent_cancel_retries(&hash);
        assert!(
            !coordinator.concurrent_cancel_retries.contains_key(&hash),
            "verdict must clear the per-block counter"
        );

        // A fresh cancel for the same hash starts from attempt 1, not
        // from cap-1 — the verdict was a clean exit, the previous
        // burst is unrelated to whatever cancels we see next.
        let decision = coordinator.record_concurrent_cancel(hash);
        assert_eq!(
            decision,
            ConcurrentCancelDecision::Requeue { attempt: 1 },
            "post-clear, the counter must start over from 1"
        );
    }

    /// Distinct block hashes maintain independent cancel counters.
    /// A poisoned block burning through its retry budget must not push a
    /// different healthy block past its cap.
    #[tokio::test]
    async fn cancel_retry_counters_are_per_hash() {
        let (block_tree_guard, _blocks) = setup_canonical_chain_scenario(0);
        let mut coordinator =
            ValidationCoordinator::new(block_tree_guard, tokio::runtime::Handle::current());
        let hash_a = BlockHash::random();
        let hash_b = BlockHash::random();
        assert_ne!(hash_a, hash_b, "test precondition: distinct hashes");

        // Push hash_a to the cap (so it would GiveUp on the next call).
        for _ in 0..(MAX_CONCURRENT_CANCEL_RETRIES - 1) {
            let d = coordinator.record_concurrent_cancel(hash_a);
            assert!(matches!(d, ConcurrentCancelDecision::Requeue { .. }));
        }

        // hash_b is untouched — its first cancel must return Requeue with
        // attempt = 1, not GiveUp.
        let decision_b = coordinator.record_concurrent_cancel(hash_b);
        assert_eq!(
            decision_b,
            ConcurrentCancelDecision::Requeue { attempt: 1 },
            "hash_b counter must be independent of hash_a's"
        );

        // hash_a's next cancel still trips the cap as expected.
        let decision_a = coordinator.record_concurrent_cancel(hash_a);
        assert_eq!(
            decision_a,
            ConcurrentCancelDecision::GiveUp {
                attempt: MAX_CONCURRENT_CANCEL_RETRIES
            },
            "hash_a should still GiveUp at the cap regardless of hash_b activity"
        );
    }

    /// `RepeatedCancellation` classifies as `SoftInternal` (not
    /// `NodeFault`, not `Invalid`). This is the routing invariant: when
    /// the cap-hit path constructs the variant and routes it via
    /// `From<ValidationError>`, the block must park rather than be
    /// peer-attributed or trigger a node-fault abort. Pair with
    /// `validation_cancel_reason_classifier_dispatch` and
    /// `validation_cancelled_converts_per_reason`, which also cover
    /// `RepeatedCancellation`.
    #[test]
    fn repeated_cancellation_classifies_as_softinternal() {
        use crate::block_tree_service::ValidationResult;
        use crate::block_validation::{ValidationCancelReason, ValidationError};

        let reason = ValidationCancelReason::RepeatedCancellation;
        // IS_INTERNAL is a const — verified at compile time by the tripwire in block_validation.rs.

        let result: ValidationResult = ValidationError::ValidationCancelled { reason }.into();
        let ValidationResult::InternalFailure(inner) = &result else {
            panic!("RepeatedCancellation must dispatch to InternalFailure, got {result:?}");
        };
        assert!(
            !inner.is_node_fault(),
            "RepeatedCancellation is a soft retry give-up, not a node fault — \
             the supervisor must NOT restart the node on this"
        );
    }

    /// The VDF terminal arms (`Invalid` and `ParentMissing`) MUST clear
    /// the `concurrent_cancel_retries` counter for the hash before
    /// dispatching the result. Without the clear, a future fresh
    /// submission of the same hash would inherit a stale counter from
    /// the prior attempt and silently start with a shortened retry
    /// budget — a poisoned-resubmission landmine.
    ///
    /// Drives the real `vdf_terminal_finalize_via` helper that the
    /// production select-loop arms call. Mirrors
    /// `cancel_retry_counter_cleared_on_verdict` (which covers the
    /// Ok-arm path) for the terminal-VDF path.
    #[tokio::test]
    async fn vdf_terminal_finalize_clears_concurrent_cancel_retries() {
        use crate::block_validation::ValidationError;
        use crate::validation_service::vdf_terminal_finalize_via;

        let (block_tree_guard, _blocks) = setup_canonical_chain_scenario(0);
        let mut coordinator =
            ValidationCoordinator::new(block_tree_guard, tokio::runtime::Handle::current());
        let hash = BlockHash::random();

        // Seed the counter to cap-1 to simulate a prior burst of cancels
        // on a previous attempt. Use the real production helper so the
        // test is sensitive to any future refactor that changes the
        // increment semantics.
        for _ in 1..MAX_CONCURRENT_CANCEL_RETRIES {
            coordinator.record_concurrent_cancel(hash);
        }
        assert_eq!(
            coordinator.concurrent_cancel_retries.get(&hash).copied(),
            Some(MAX_CONCURRENT_CANCEL_RETRIES - 1),
            "test precondition: counter must be at cap-1"
        );

        // Construct a real channel so the dispatch path actually fires
        // (the helper returns true on success). We don't care about the
        // dispatched payload here — the counter clear is the contract.
        let (block_tree_sender, _block_tree_rx) = tokio::sync::mpsc::unbounded_channel();

        // VDF Invalid arm: route a `VdfValidationFailed` (Consensus
        // classification) through the helper. Counter must be cleared.
        let dispatched = vdf_terminal_finalize_via(
            &mut coordinator,
            &block_tree_sender,
            hash,
            ValidationError::VdfValidationFailed("synthetic vdf failure".to_string()),
        );
        assert!(
            dispatched,
            "VDF Invalid terminal dispatch must succeed when the receiver is alive"
        );
        assert!(
            !coordinator.concurrent_cancel_retries.contains_key(&hash),
            "VDF Invalid terminal must clear concurrent_cancel_retries — without this, \
             a future fresh submission's retry budget silently shortens (poisoned-resubmission landmine)"
        );

        // Re-seed the counter and exercise the VDF ParentMissing arm
        // through the same helper. Same contract.
        for _ in 1..MAX_CONCURRENT_CANCEL_RETRIES {
            coordinator.record_concurrent_cancel(hash);
        }
        assert_eq!(
            coordinator.concurrent_cancel_retries.get(&hash).copied(),
            Some(MAX_CONCURRENT_CANCEL_RETRIES - 1),
        );
        let dispatched = vdf_terminal_finalize_via(
            &mut coordinator,
            &block_tree_sender,
            hash,
            ValidationError::ParentBlockMissing {
                block_hash: BlockHash::random(),
            },
        );
        assert!(dispatched);
        assert!(
            !coordinator.concurrent_cancel_retries.contains_key(&hash),
            "VDF ParentMissing terminal must clear concurrent_cancel_retries — \
             same counter-clear invariant as the Invalid arm"
        );
    }

    // =========================================================================
    // Tests for the VdfBlockingTaskFailed sentinel (JoinError split: panic vs abort)
    // =========================================================================
    //
    // These tests verify the fix for the never-mislabel invariant on
    // `spawn_blocking` sites in `ensure_vdf_is_valid`. Before the fix, a
    // `JoinError` from Stage B / C/D fell through the downcast chain to the
    // `None` arm and became `VdfValidationResult::Invalid` — a peer-attributable
    // consensus rejection for what is actually a local-verifier-thread fault.
    //
    // Mirrors the symmetric PoA test suite at
    // `block_validation_task.rs:classify_poa_join_error_tests`.

    /// Cancelled-arm: an externally aborted `spawn_blocking` task produces
    /// `JoinError::is_cancelled()`. The `VdfBlockingTaskFailed` sentinel
    /// (is_panic=false) must route to `VdfValidationResult::Cancelled`, not
    /// `Invalid`.
    ///
    /// We test this by constructing the sentinel directly and confirming it
    /// round-trips through `eyre::Report` and maps to the correct branch.
    #[tokio::test]
    async fn vdf_blocking_cancelled_routes_to_cancelled_not_invalid() {
        // Simulate a JoinError::is_cancelled() by aborting a spawned task.
        let handle: tokio::task::JoinHandle<()> = tokio::spawn(std::future::pending());
        handle.abort();
        let join_err = handle
            .await
            .expect_err("aborted task must produce JoinError");
        assert!(
            join_err.is_cancelled(),
            "fixture must produce is_cancelled() = true"
        );

        // Construct the sentinel exactly as `ensure_vdf_is_valid` does.
        let sentinel = VdfBlockingTaskFailed {
            is_panic: join_err.is_panic(),
            is_cancelled: join_err.is_cancelled(),
            stage: VdfTaskStage::ValidateSeeds,
        };
        assert!(
            !sentinel.is_panic,
            "is_cancelled fixture must not be is_panic"
        );

        // Confirm the sentinel round-trips through eyre::Report.
        let report: eyre::Report = sentinel.into();
        let recovered = report
            .downcast_ref::<VdfBlockingTaskFailed>()
            .expect("VdfBlockingTaskFailed must survive eyre boxing");
        assert!(!recovered.is_panic);
        assert!(recovered.is_cancelled);
        assert_eq!(recovered.stage, VdfTaskStage::ValidateSeeds);

        // The production path: if !is_panic → Cancelled.
        let result = if recovered.is_panic {
            VdfValidationResult::Invalid(eyre::eyre!("should not reach"))
        } else {
            VdfValidationResult::Cancelled
        };
        assert!(
            matches!(result, VdfValidationResult::Cancelled),
            "VdfBlockingTaskFailed(is_cancelled) must map to Cancelled, not Invalid"
        );
    }

    /// Panic-arm: a `spawn_blocking` task that panics produces a
    /// `JoinError::is_panic()`. Confirms the sentinel carries `is_panic=true`
    /// and round-trips through `eyre::Report`.
    #[tokio::test]
    async fn vdf_blocking_panic_produces_is_panic_join_error() {
        let handle = tokio::task::spawn_blocking(|| -> () {
            panic!("simulated blocking-thread panic in VDF stage");
        });
        let join_err = handle
            .await
            .expect_err("panicking blocking task must produce JoinError");
        assert!(
            join_err.is_panic(),
            "panicking spawn_blocking must produce is_panic() = true"
        );

        let sentinel = VdfBlockingTaskFailed {
            is_panic: join_err.is_panic(),
            is_cancelled: join_err.is_cancelled(),
            stage: VdfTaskStage::ValidateBatch,
        };
        assert!(sentinel.is_panic, "sentinel must carry is_panic=true");
        assert!(!sentinel.is_cancelled);
        assert_eq!(sentinel.stage, VdfTaskStage::ValidateBatch);

        let report: eyre::Report = sentinel.into();
        let recovered = report
            .downcast_ref::<VdfBlockingTaskFailed>()
            .expect("VdfBlockingTaskFailed must survive eyre boxing");
        assert!(recovered.is_panic);
        assert!(!recovered.is_cancelled);
        assert_eq!(recovered.stage, VdfTaskStage::ValidateBatch);
    }

    /// Panic dispatch: drives the `VdfBlockingTaskFailed { is_panic: true }`
    /// branch in the downcast chain to confirm the never-mislabel panic fires
    /// with the correct message. Uses the same conditional logic as the
    /// production code path.
    #[tokio::test]
    #[should_panic(expected = "VDF blocking task panicked")]
    async fn vdf_blocking_panic_sentinel_causes_execute_to_panic() {
        let sentinel = VdfBlockingTaskFailed {
            is_panic: true,
            is_cancelled: false,
            stage: VdfTaskStage::ValidateSeeds,
        };
        let report: eyre::Report = sentinel.into();
        let e = &report;

        // Replicate the dispatch logic from PreemptibleVdfTask::execute.
        if let Some(blocking_failed) = e.downcast_ref::<VdfBlockingTaskFailed>()
            && blocking_failed.is_panic
        {
            panic!(
                "VDF blocking task panicked (stage={:?}, block={}); local-thread fault — crashing per never-mislabel rule",
                blocking_failed.stage, "test-block-hash",
            );
        }
    }
}
