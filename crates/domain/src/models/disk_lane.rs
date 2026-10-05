//! Disk lane for one storage module.
//!
//! Callers enqueue. This lane decides when `chunks.dat` is locked.
//! Order: mining recall, then the longer ready command. That command is a
//! packed write or an entropy read. A mining recall issues its `pread`s
//! immediately. Packed writes already in the kernel keep running beside
//! those `pread`s. The lane submits no new write until the recall drops
//! the disk. It sorts a reorder buffer of pending chunks and joins adjacent
//! chunks into one `pwrite` of at most `WRITE_RUN_MAX_BYTES`. Contiguous
//! chunks from one entropy span enter that buffer together, after every
//! index ack for those chunks. A caller that already holds neighboring
//! chunks publishes that run in one insert, so the lane cannot read a
//! prefix of it. A short run waits for that size, a recall flush, the
//! pending set at `num_writes_before_sync`, or `reorder_grace` only while
//! the chunk disk is busy. One busy pass writes every full run and the
//! oldest aged short run. That short run goes first. The next short run
//! waits another `reorder_grace` while the disk stays busy, so the others
//! can still gain a neighbor. An idle disk issues the short runs already
//! queued. It ranks them with the full runs and does not arm the hold.
//! Entropy reads use the same rule on their own queue and their own hold.
//! A short span waits for the sweep cap, the disk-slot count at
//! `num_writes_before_sync`, a recall flush, or `reorder_grace` only while
//! the chunk disk is busy. One busy pass reads every full span and the
//! oldest aged short span. That short span is read first. Further short
//! spans wait another grace while the disk stays busy. An idle disk issues
//! the short spans already queued, ranked with the full spans, and does
//! not arm the read hold. The write hold and the read hold stay separate,
//! so a short write does not block a ready read.
//! New submits stop when another command would put the in-flight set past
//! 500 ms, or when `INFLIGHT_WRITES` commands are already out.
//! The file lock is dropped before an index submit.
//! Index commits stay off this drive while a chunk `pread`, `pwrite`,
//! `fsync`, or mining recall is in progress. They run in the gap before
//! the next chunk command.
//! This thread writes `intervals.json` for the submodules in a batch before
//! those `pwrite`s and again after their `fsync`. Another thread posts that
//! write here. The index database still commits on its own path.
//!
//! `StorageModuleService` starts one thread per module. That thread runs the
//! sweep and the packed flush. It sleeps on the gate condvar and wakes when
//! work is queued, a call finishes, a recall hold changes, an index ack
//! arrives, or another thread posts a flush or an interval write. Ingress
//! and data sync wait on the group result and do not lock the file. A mining
//! recall still `pread`s on the mining thread and keeps two of those calls
//! in the kernel. This thread submits no new write while the recall count
//! is set. Writes already handed to the kernel run on.

use std::{
    collections::HashMap,
    fs::File,
    ops::Deref,
    sync::{
        Arc, Condvar, Mutex, MutexGuard, PoisonError, TryLockError,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, TryRecvError},
    },
    time::{Duration, Instant},
};

use irys_packing::packing_xor_vec_u8;
use irys_types::{ChunkPathHash, PartitionChunkOffset, UnpackedChunk};
use nodit::{InclusiveInterval as _, Interval};
use std::os::unix::fs::FileExt as _;

use super::{
    BatchEnqueueItem, ChunkType, StorageModule, WriteDataChunkError, WriteRun,
    index_drain::{ChunkBusy, IndexGap, IndexOp},
};

/// How long a caller waits for its entropy read before it gives up.
const SWEEP_WAIT: Duration = Duration::from_secs(30);
/// No-lane packed flush still commits at most this many runs.
const INFLIGHT_DISK_OPS: usize = 3;
/// Commands one storage module may have in the kernel at once. This is the
/// block-queue depth. The service-time budget can stop earlier.
const INFLIGHT_WRITES: usize = 64;
/// Pending chunks sorted by offset before one flush. The scan has to be at
/// least as wide as the durability count, or later chunks are never joined.
pub(super) const WRITE_REORDER_CHUNKS: usize = 10_000;
/// Largest packed `pwrite`. One entropy `pread` uses `entropy_sweep_max_bytes`.
pub(super) const WRITE_RUN_MAX_BYTES: u64 = 10 * 1024 * 1024;
/// Head movement charged for one more in-flight command.
const SEEK_US: u64 = 2_600;
/// Quiet-drive media rate used to weigh in-flight bytes.
const MEDIA_BYTES_PER_SEC: u64 = 182 * 1024 * 1024;
/// In-flight packed writes and entropy reads must fit in this service time.
const SERVICE_BUDGET_US: u64 = 500_000;

fn service_us(count: u64, bytes: u64) -> u64 {
    let transfer = bytes.saturating_mul(1_000_000) / MEDIA_BYTES_PER_SEC;
    count.saturating_mul(SEEK_US).saturating_add(transfer)
}

/// Wait before a short run may jump a longer one.
///
/// One capped run costs about `service_us(1, WRITE_RUN_MAX_BYTES)`. That
/// interval is too short to use as the wait: a short run would flush before
/// a neighbor was queued, and at a full reorder buffer every chunk would
/// already be older than one run. This wait is the service time of one
/// reorder scan filled with capped runs, so neighbors can still join.
pub(super) fn reorder_grace(chunk_size: u64) -> Duration {
    let chunk = chunk_size.max(1);
    let per_run = (WRITE_RUN_MAX_BYTES / chunk).max(1);
    let runs = (WRITE_REORDER_CHUNKS as u64 / per_run).max(1);
    let run_bytes = WRITE_RUN_MAX_BYTES.min(per_run.saturating_mul(chunk));
    Duration::from_micros(service_us(runs, runs.saturating_mul(run_bytes)))
}

type SweepNotify = tokio::sync::oneshot::Sender<Result<(), WriteDataChunkError>>;

/// Condvar shared by the lane, recall waiters, and index acks.
/// `epoch` advances under `mu` on every wake. A waiter that reads it
/// under `mu` before waiting cannot miss a wake that already ran.
#[derive(Clone)]
pub(super) struct DiskWake {
    inner: Arc<DiskWakeInner>,
}

struct DiskWakeInner {
    mu: Mutex<()>,
    cv: Condvar,
    epoch: AtomicU64,
}

impl DiskWake {
    fn new() -> Self {
        Self {
            inner: Arc::new(DiskWakeInner {
                mu: Mutex::new(()),
                cv: Condvar::new(),
                epoch: AtomicU64::new(0),
            }),
        }
    }

    pub(super) fn notify(&self) {
        let _guard = self.lock();
        self.notify_locked();
    }

    fn notify_locked(&self) {
        self.inner.epoch.fetch_add(1, Ordering::SeqCst);
        self.inner.cv.notify_all();
    }

    fn lock(&self) -> MutexGuard<'_, ()> {
        self.inner.mu.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn epoch(&self) -> u64 {
        self.inner.epoch.load(Ordering::SeqCst)
    }

    fn wait<'a>(&self, guard: MutexGuard<'a, ()>) -> MutexGuard<'a, ()> {
        self.inner
            .cv
            .wait(guard)
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn wait_timeout<'a>(&self, guard: MutexGuard<'a, ()>, timeout: Duration) -> MutexGuard<'a, ()> {
        self.inner
            .cv
            .wait_timeout(guard, timeout)
            .unwrap_or_else(PoisonError::into_inner)
            .0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum WritePriority {
    Migration,
    Packing,
    Ingress,
}

#[derive(Debug)]
struct SweepGroup {
    remaining: usize,
    failed: Option<WriteDataChunkError>,
    /// When set, the group stays until the waiter takes the result.
    waited: bool,
    /// Set when the caller is waiting asynchronously. Completion sends
    /// the result and removes the group.
    notify: Option<SweepNotify>,
}

#[derive(Debug)]
struct SweepSlot {
    group: u64,
    offset: PartitionChunkOffset,
    unpacked: Arc<Vec<u8>>,
    data_path: Arc<Vec<u8>>,
    path_hash: ChunkPathHash,
    generation: u64,
    priority: WritePriority,
    pending_entropy: Option<Vec<u8>>,
    byte_len: u64,
    /// When this read was queued. A short span uses the oldest slot as its clock.
    queued_at: Instant,
}

/// One body whose entropy targets are reserved and not yet in the sweep queue.
struct OccupiedChunk {
    unpacked: Arc<Vec<u8>>,
    data_path: Arc<Vec<u8>>,
    path_hash: ChunkPathHash,
    priority: WritePriority,
    waited: bool,
    notify: Option<SweepNotify>,
    selected: Vec<(PartitionChunkOffset, u64, Option<Vec<u8>>)>,
}

enum OccupyFailure {
    Write(WriteDataChunkError),
    /// The offset already has an index write reserved.
    InFlight,
}

struct Inflight {
    group: u64,
    offset: PartitionChunkOffset,
    packed: Vec<u8>,
    generation: u64,
    priority: WritePriority,
    byte_len: u64,
    /// Set when this chunk is part of a contiguous island. The chunk stays
    /// out of the reorder buffer until every member of that island has an ack.
    release_id: Option<u64>,
    done: Receiver<Result<(), WriteDataChunkError>>,
}

/// One packed chunk held until the rest of its island has an index ack.
struct ReleasedChunk {
    group: u64,
    offset: PartitionChunkOffset,
    packed: Vec<u8>,
    generation: u64,
    priority: WritePriority,
    byte_len: u64,
}

/// Packed chunks from one contiguous island, waiting for the remaining acks.
struct SpanRelease {
    left: usize,
    ready: Vec<ReleasedChunk>,
}

enum AccountedRelease {
    Missing(Option<ReleasedChunk>),
    Open,
    Finished(SpanRelease),
}

#[derive(Default)]
struct SweepQueue {
    slots: Vec<SweepSlot>,
    groups: HashMap<u64, SweepGroup>,
    next_group: u64,
    inflight: Vec<Inflight>,
    next_release: u64,
    releases: HashMap<u64, SpanRelease>,
}

/// Wakeups for recall vs write vs entropy. `chunks.dat` is locked only after
/// these counters allow the hold.
pub(super) struct DiskGate {
    recall_waiters: AtomicUsize,
    write_holds: AtomicUsize,
    /// `pread` / `pwrite` calls already handed to the kernel.
    disk_ops: AtomicUsize,
    /// Bytes in those calls. The service-time budget weighs this with `disk_ops`.
    inflight_bytes: AtomicU64,
    /// Callers that hold `chunks.dat` for the whole syscall. Inflight ops dup
    /// the fd and do not take this.
    io_exclusive: AtomicUsize,
    /// A packed-write window is owed before the next recall may take the disk.
    recall_flush_owed: AtomicBool,
    wake: DiskWake,
    queue: Mutex<SweepQueue>,
    sweep_busy: Mutex<()>,
    /// The service thread owns sweeps and flushes while this is set.
    lane_started: AtomicBool,
    lane_stop: AtomicBool,
    /// Index commits wait while a chunk run or a mining recall holds this.
    index_gap: Arc<IndexGap>,
    /// Further short runs wait until this instant. Full runs ignore it.
    short_hold_until: Mutex<Option<Instant>>,
    /// Further short entropy spans wait until this instant. Full spans ignore it.
    read_short_hold_until: Mutex<Option<Instant>>,
    /// Set for the life of `run_disk_lane`. External flushes use it to avoid
    /// waiting for themselves.
    lane_thread: Mutex<Option<std::thread::ThreadId>>,
    /// External flush and interval writes wait here. The lane thread runs them.
    handoff: LaneHandoff,
    #[cfg(test)]
    last_commit_thread: Mutex<Option<std::thread::ThreadId>>,
    #[cfg(test)]
    pub(super) entropy_preads: AtomicU64,
    /// Non-recall `pread`s from `read_chunks`. One contiguous disk run is one
    /// call when the run fits the recall range.
    #[cfg(test)]
    pub(super) source_preads: AtomicU64,
    /// Recall runs whose clean pages were dropped after the bytes were copied.
    #[cfg(test)]
    pub(super) recall_cache_drops: AtomicU64,
    /// Sweep-queue publishes. One contiguous run is one publish.
    #[cfg(test)]
    pub(super) enqueue_notifies: AtomicU64,
}

/// One posted flush or interval write, and the result the waiter collects.
struct LaneHandoff {
    mu: Mutex<LaneHandoffState>,
    cv: Condvar,
}

struct LaneHandoffState {
    next: u64,
    completed_until: u64,
    /// Exclusive end. A force flush is owed while this is past `completed_until`.
    force_through: u64,
    /// Exclusive end. A full interval rewrite is owed while this is past
    /// `completed_until`.
    persist_through: u64,
    outcome: HashMap<u64, LaneOutcome>,
}

enum LaneOutcome {
    Done,
    Failed(String),
}

struct LaneRequest {
    /// Tickets in `completed_until..mark` finish with this request.
    mark: u64,
    force: bool,
    persist: bool,
}

/// The lane did not run the posted work, or the work failed.
pub(super) enum LaneHandoffError {
    /// The lane is stopping or already gone. The caller writes on its own thread.
    Stopped,
    Failed(String),
}

impl LaneHandoff {
    fn new() -> Self {
        Self {
            mu: Mutex::new(LaneHandoffState {
                next: 0,
                completed_until: 0,
                force_through: 0,
                persist_through: 0,
                outcome: HashMap::new(),
            }),
            cv: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, LaneHandoffState> {
        self.mu.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn post(&self, force: bool, persist: bool) -> u64 {
        let mut state = self.lock();
        let ticket = state.next;
        state.next = state.next.saturating_add(1);
        if force {
            state.force_through = state.next;
        }
        if persist {
            state.persist_through = state.next;
        }
        self.cv.notify_all();
        ticket
    }

    fn pending(&self) -> bool {
        let state = self.lock();
        state.completed_until < state.next
            && (state.force_through > state.completed_until
                || state.persist_through > state.completed_until)
    }

    fn take(&self) -> Option<LaneRequest> {
        let mut state = self.lock();
        if state.completed_until >= state.next {
            return None;
        }
        let force = state.force_through > state.completed_until;
        let persist = state.persist_through > state.completed_until;
        if !force && !persist {
            return None;
        }
        let mark = state.next;
        // Drop the watermark we are about to run. A post during the work
        // raises it again and the next take sees that request.
        if force {
            state.force_through = state.completed_until;
        }
        if persist {
            state.persist_through = state.completed_until;
        }
        Some(LaneRequest {
            mark,
            force,
            persist,
        })
    }

    fn finish(&self, mark: u64, error: Option<String>) {
        let mut state = self.lock();
        let start = state.completed_until;
        if mark > start {
            for ticket in start..mark {
                let stored = match &error {
                    Some(error) => LaneOutcome::Failed(error.clone()),
                    None => LaneOutcome::Done,
                };
                state.outcome.insert(ticket, stored);
            }
            state.completed_until = mark;
        }
        self.cv.notify_all();
    }

    fn wait(&self, ticket: u64, lane_started: &AtomicBool) -> Result<(), LaneHandoffError> {
        let mut state = self.lock();
        loop {
            if let Some(outcome) = state.outcome.remove(&ticket) {
                return match outcome {
                    LaneOutcome::Done => Ok(()),
                    LaneOutcome::Failed(error) => Err(LaneHandoffError::Failed(error)),
                };
            }
            if !lane_started.load(Ordering::Acquire) {
                return Err(LaneHandoffError::Stopped);
            }
            state = self.cv.wait(state).unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn wake(&self) {
        let _state = self.lock();
        self.cv.notify_all();
    }
}

/// Decrements `disk_ops` and `inflight_bytes` when the kernel call returns.
struct OpGuard<'a> {
    disk: &'a DiskGate,
    bytes: u64,
}

/// File guard drops first, then the exclusive count. A new op must not start
/// while this guard still holds the mutex.
pub(super) struct ChunkFile<'a> {
    file: MutexGuard<'a, File>,
    _exclusive: ClearExclusive<'a>,
}

struct ClearExclusive<'a> {
    disk: &'a DiskGate,
}

struct SpanLoad {
    start: PartitionChunkOffset,
    buf: Vec<u8>,
}

pub(super) struct WriteHold<'a> {
    gate: &'a DiskGate,
}

/// Drops the recall count even when the read returns with an error.
/// The index pin drops with it, after the `pread`s have returned.
pub(super) struct RecallHold<'a> {
    gate: &'a DiskGate,
    _index: ChunkBusy,
}

impl std::fmt::Debug for DiskGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskGate")
            .field("recall_waiters", &self.recall_waiters)
            .field("write_holds", &self.write_holds)
            .field("disk_ops", &self.disk_ops.load(Ordering::SeqCst))
            .field("io_exclusive", &self.io_exclusive.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl Deref for ChunkFile<'_> {
    type Target = File;

    fn deref(&self) -> &Self::Target {
        &self.file
    }
}

impl Drop for OpGuard<'_> {
    fn drop(&mut self) {
        self.disk.finish_op(self.bytes);
    }
}

impl Drop for ClearExclusive<'_> {
    fn drop(&mut self) {
        self.disk.end_exclusive_io();
    }
}

impl DiskGate {
    pub(super) fn new() -> Self {
        Self::with_index_gap(Arc::new(IndexGap::new()))
    }

    pub(super) fn with_index_gap(index_gap: Arc<IndexGap>) -> Self {
        Self {
            recall_waiters: AtomicUsize::new(0),
            write_holds: AtomicUsize::new(0),
            disk_ops: AtomicUsize::new(0),
            inflight_bytes: AtomicU64::new(0),
            io_exclusive: AtomicUsize::new(0),
            recall_flush_owed: AtomicBool::new(false),
            wake: DiskWake::new(),
            queue: Mutex::new(SweepQueue::default()),
            sweep_busy: Mutex::new(()),
            lane_started: AtomicBool::new(false),
            lane_stop: AtomicBool::new(false),
            index_gap,
            short_hold_until: Mutex::new(None),
            read_short_hold_until: Mutex::new(None),
            lane_thread: Mutex::new(None),
            handoff: LaneHandoff::new(),
            #[cfg(test)]
            last_commit_thread: Mutex::new(None),
            #[cfg(test)]
            entropy_preads: AtomicU64::new(0),
            #[cfg(test)]
            source_preads: AtomicU64::new(0),
            #[cfg(test)]
            recall_cache_drops: AtomicU64::new(0),
            #[cfg(test)]
            enqueue_notifies: AtomicU64::new(0),
        }
    }

    pub(super) fn bind_lane_thread(&self) {
        let mut guard = self
            .lane_thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *guard = Some(std::thread::current().id());
    }

    pub(super) fn unbind_lane_thread(&self) {
        let mut guard = self
            .lane_thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *guard = None;
    }

    /// True on the thread that is inside `run_disk_lane`.
    pub(super) fn on_lane_thread(&self) -> bool {
        let guard = self
            .lane_thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        guard.as_ref() == Some(&std::thread::current().id())
    }

    #[cfg(test)]
    pub(super) fn lane_thread_id(&self) -> Option<std::thread::ThreadId> {
        let guard = self
            .lane_thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *guard
    }

    #[cfg(test)]
    pub(super) fn record_commit_thread(&self) {
        let mut guard = self
            .last_commit_thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *guard = Some(std::thread::current().id());
    }

    #[cfg(test)]
    pub(super) fn last_commit_thread_id(&self) -> Option<std::thread::ThreadId> {
        let guard = self
            .last_commit_thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *guard
    }

    /// Ask the lane to drain every pending run. `Stopped` means this caller
    /// must drain them itself. The wait does not take the sync lock.
    pub(super) fn request_lane_force(&self) -> Result<(), LaneHandoffError> {
        self.request_lane(true, false)
    }

    /// Ask the lane to rewrite every `intervals.json`. `Stopped` means this
    /// caller must write the files itself.
    pub(super) fn request_lane_persist(&self) -> Result<(), LaneHandoffError> {
        self.request_lane(false, true)
    }

    fn request_lane(&self, force: bool, persist: bool) -> Result<(), LaneHandoffError> {
        if self.on_lane_thread() || !self.lane_running() {
            return Err(LaneHandoffError::Stopped);
        }
        let ticket = self.handoff.post(force, persist);
        self.notify();
        self.handoff.wait(ticket, &self.lane_started)
    }

    /// The next pass owes an external flush or a full interval rewrite.
    pub(super) fn lane_request_pending(&self) -> bool {
        self.handoff.pending()
    }

    fn take_lane_request(&self) -> Option<LaneRequest> {
        self.handoff.take()
    }

    fn finish_lane_request(&self, mark: u64, error: Option<String>) {
        self.handoff.finish(mark, error);
    }

    fn wake_lane_handoff(&self) {
        self.handoff.wake();
    }

    /// True while a short run written earlier still owns the grace window.
    pub(super) fn short_writes_held(&self) -> bool {
        self.short_hold_remaining().is_some()
    }

    /// Time until another short run may be written. `None` when the hold
    /// is unset or already due.
    pub(super) fn short_hold_remaining(&self) -> Option<Duration> {
        let guard = self
            .short_hold_until
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let until = (*guard)?;
        let now = Instant::now();
        if now < until {
            Some(until.saturating_duration_since(now))
        } else {
            None
        }
    }

    /// Block further short runs for `grace` after one short run is written.
    pub(super) fn arm_short_hold(&self, grace: Duration) {
        let mut guard = self
            .short_hold_until
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *guard = Instant::now().checked_add(grace);
    }

    /// True while a short entropy span read earlier still owns the grace window.
    pub(super) fn short_reads_held(&self) -> bool {
        self.read_short_hold_remaining().is_some()
    }

    /// Time until another short entropy span may be read. `None` when the
    /// hold is unset or already due.
    pub(super) fn read_short_hold_remaining(&self) -> Option<Duration> {
        let guard = self
            .read_short_hold_until
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let until = (*guard)?;
        let now = Instant::now();
        if now < until {
            Some(until.saturating_duration_since(now))
        } else {
            None
        }
    }

    /// Block further short entropy spans for `grace` after one short span is read.
    pub(super) fn arm_read_short_hold(&self, grace: Duration) {
        let mut guard = self
            .read_short_hold_until
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *guard = Instant::now().checked_add(grace);
    }

    /// Returns false when a lane thread is already marked running.
    pub(super) fn prepare_lane(&self) -> bool {
        self.lane_stop.store(false, Ordering::Release);
        self.lane_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(super) fn clear_lane(&self) {
        self.lane_started.store(false, Ordering::Release);
    }

    pub(super) fn lane_running(&self) -> bool {
        self.lane_started.load(Ordering::Acquire)
    }

    pub(super) fn request_stop(&self) {
        self.lane_stop.store(true, Ordering::Release);
        self.notify();
    }

    pub(super) fn stop_requested(&self) -> bool {
        self.lane_stop.load(Ordering::Acquire)
    }

    pub(super) fn notify(&self) {
        self.wake.notify();
    }

    fn sweep_lock_free(&self) -> bool {
        match self.sweep_busy.try_lock() {
            Ok(guard) => {
                drop(guard);
                true
            }
            Err(TryLockError::WouldBlock) => false,
            Err(TryLockError::Poisoned(poisoned)) => {
                drop(poisoned.into_inner());
                true
            }
        }
    }

    fn lock_wake(&self) -> MutexGuard<'_, ()> {
        self.wake.lock()
    }

    /// Recall, a file holder, and a full service budget all block a new submit.
    fn disk_available(&self) -> bool {
        self.recall_waiters.load(Ordering::SeqCst) == 0
            && self.write_holds.load(Ordering::SeqCst) == 0
            && self.io_exclusive.load(Ordering::SeqCst) == 0
            && self.disk_ops.load(Ordering::SeqCst) < INFLIGHT_WRITES
            && self.budget_allows(self.disk_ops.load(Ordering::SeqCst), 0)
    }

    /// True when no chunk command is in the kernel and no recall, write hold,
    /// or exclusive IO owns the disk. A short command may wait for a neighbor
    /// only while this is false. Room left in the service budget is still busy:
    /// the in-flight command keeps the disk active, and the short command can grow.
    pub(super) fn chunk_disk_idle(&self) -> bool {
        self.disk_ops.load(Ordering::SeqCst) == 0
            && self.recall_waiters.load(Ordering::SeqCst) == 0
            && self.write_holds.load(Ordering::SeqCst) == 0
            && self.io_exclusive.load(Ordering::SeqCst) == 0
    }

    /// One tiny in-flight write. Tests use it so a short command stays queued.
    #[cfg(test)]
    pub(super) fn occupy_for_test(&self) {
        assert!(
            self.try_submit_write(INFLIGHT_WRITES, 1),
            "occupy the chunk disk for a test"
        );
    }

    /// Drops the command from [`Self::occupy_for_test`] and wakes the lane.
    #[cfg(test)]
    pub(super) fn release_for_test(&self) {
        self.finish_op(1);
    }

    pub(super) fn begin_recall(&self) -> RecallHold<'_> {
        // Pin before the owed flush waits. The `pread` must not wait for an
        // index commit, and a commit must not start under the `pread`.
        let index = self.index_gap.pin_recall();
        let mut guard = self.lock_wake();
        // Do not count this recall yet. The owed window has to submit, and a
        // recall waiter would make that submit wait for this recall.
        while self.recall_flush_owed.load(Ordering::SeqCst) {
            guard = self.wake.wait(guard);
        }
        self.recall_waiters.fetch_add(1, Ordering::SeqCst);
        self.wake.notify_locked();
        drop(guard);
        RecallHold {
            gate: self,
            _index: index,
        }
    }

    /// Block the next recall until one packed-write window commits.
    pub(super) fn arm_recall_flush(&self) {
        let _guard = self.lock_wake();
        self.recall_flush_owed.store(true, Ordering::SeqCst);
        self.wake.notify_locked();
    }

    pub(super) fn clear_recall_flush(&self) {
        let _guard = self.lock_wake();
        self.recall_flush_owed.store(false, Ordering::SeqCst);
        self.wake.notify_locked();
    }

    pub(super) fn recall_flush_is_owed(&self) -> bool {
        self.recall_flush_owed.load(Ordering::SeqCst)
    }

    /// Wait until the owed packed-write window has committed.
    pub(super) fn wait_recall_flush(&self) {
        let mut guard = self.lock_wake();
        while self.recall_flush_owed.load(Ordering::SeqCst) {
            guard = self.wake.wait(guard);
        }
    }

    /// Reserve one kernel call of `next_bytes`. Fails when a recall is waiting,
    /// someone holds the file, the count cap is full, or the service budget
    /// cannot take another command. An idle disk takes one command anyway.
    /// A write hold also blocks a read: the flush already owns the disk.
    fn try_submit_read(&self, limit: usize, next_bytes: u64) -> bool {
        self.try_submit(limit, true, next_bytes)
    }

    fn try_submit_write(&self, limit: usize, next_bytes: u64) -> bool {
        self.try_submit(limit, false, next_bytes)
    }

    fn try_submit(&self, limit: usize, is_read: bool, next_bytes: u64) -> bool {
        let _guard = self.lock_wake();
        if self.submit_blocked(limit, is_read, next_bytes) {
            return false;
        }
        self.disk_ops.fetch_add(1, Ordering::SeqCst);
        self.inflight_bytes.fetch_add(next_bytes, Ordering::SeqCst);
        true
    }

    fn submit_blocked(&self, limit: usize, is_read: bool, next_bytes: u64) -> bool {
        let count = self.disk_ops.load(Ordering::SeqCst);
        self.recall_waiters.load(Ordering::SeqCst) > 0
            || self.io_exclusive.load(Ordering::SeqCst) > 0
            || count >= limit
            || (is_read && self.write_holds.load(Ordering::SeqCst) > 0)
            || !self.budget_allows(count, next_bytes)
    }

    /// `count == 0` admits the command. Otherwise count and bytes together
    /// must stay inside `SERVICE_BUDGET_US`.
    fn budget_allows(&self, count: usize, next_bytes: u64) -> bool {
        if count == 0 {
            return true;
        }
        if count >= INFLIGHT_WRITES {
            return false;
        }
        let bytes = self
            .inflight_bytes
            .load(Ordering::SeqCst)
            .saturating_add(next_bytes);
        service_us(count as u64 + 1, bytes) <= SERVICE_BUDGET_US
    }

    fn finish_op(&self, bytes: u64) {
        let _guard = self.lock_wake();
        let prev = self.disk_ops.fetch_sub(1, Ordering::SeqCst);
        debug_assert!(prev > 0);
        let prev_bytes = self.inflight_bytes.fetch_sub(bytes, Ordering::SeqCst);
        debug_assert!(prev_bytes >= bytes);
        self.wake.notify_locked();
    }

    fn begin_exclusive_io(&self) {
        let guard = self.lock_wake();
        self.io_exclusive.fetch_add(1, Ordering::SeqCst);
        self.wake.notify_locked();
        self.wait_for_disk_ops(guard);
    }

    fn end_exclusive_io(&self) {
        let _guard = self.lock_wake();
        let prev = self.io_exclusive.fetch_sub(1, Ordering::SeqCst);
        debug_assert!(prev > 0);
        self.wake.notify_locked();
    }

    /// Dup is not enough for paths that keep the mutex across the syscall.
    /// The mutex is taken only after inflight calls have returned.
    pub(super) fn lock_chunks<'a>(&'a self, file: &'a Arc<Mutex<File>>) -> ChunkFile<'a> {
        self.begin_exclusive_io();
        ChunkFile {
            file: file.lock().unwrap_or_else(PoisonError::into_inner),
            _exclusive: ClearExclusive { disk: self },
        }
    }

    /// Block until kernel calls already submitted have returned.
    fn wait_until_idle(&self) {
        let guard = self.lock_wake();
        self.wait_for_disk_ops(guard);
    }

    fn wait_for_disk_ops(&self, mut guard: MutexGuard<'_, ()>) {
        while self.disk_ops.load(Ordering::SeqCst) > 0 {
            guard = self.wake.wait(guard);
        }
    }

    /// Block until `next_bytes` can be submitted, or a recall is waiting.
    /// The check and the wait share the wake lock.
    fn wait_for_write_slot(&self, next_bytes: u64) {
        let mut guard = self.lock_wake();
        while self.recall_waiters.load(Ordering::SeqCst) == 0
            && self.submit_blocked(INFLIGHT_WRITES, false, next_bytes)
        {
            guard = self.wake.wait(guard);
        }
    }

    pub(super) fn hold_writes(&self) -> WriteHold<'_> {
        let _guard = self.lock_wake();
        self.write_holds.fetch_add(1, Ordering::SeqCst);
        self.wake.notify_locked();
        WriteHold { gate: self }
    }

    /// Wait until no mining recall is queued or in progress.
    pub(super) fn yield_to_recall(&self) {
        let mut guard = self.lock_wake();
        while self.recall_waiters.load(Ordering::SeqCst) > 0 {
            guard = self.wake.wait(guard);
        }
    }

    /// Wait until mining recall is clear, or until `deadline`.
    /// Returns false when the deadline passes while a recall still holds the disk.
    pub(super) fn yield_to_recall_until(&self, deadline: Instant) -> bool {
        let mut guard = self.lock_wake();
        while self.recall_waiters.load(Ordering::SeqCst) > 0 {
            let Some(remain) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            if remain.is_zero() {
                return false;
            }
            guard = self.wake.wait_timeout(guard, remain);
        }
        true
    }

    /// Wait until a packed-chunk flush releases the disk.
    pub(super) fn yield_to_writes(&self) {
        let mut guard = self.lock_wake();
        while self.write_holds.load(Ordering::SeqCst) > 0 {
            guard = self.wake.wait(guard);
        }
    }

    pub(super) fn recall_pending(&self) -> bool {
        self.recall_waiters.load(Ordering::SeqCst) > 0
    }

    fn writes_active(&self) -> bool {
        self.write_holds.load(Ordering::SeqCst) > 0
    }

    fn queue(&self) -> std::sync::MutexGuard<'_, SweepQueue> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn try_sweep_busy(&self) -> Option<SweepBusy<'_>> {
        let wake = self.wake.clone();
        let guard = match self.sweep_busy.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::WouldBlock) => return None,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        };
        Some(SweepBusy {
            wake,
            guard: Some(guard),
        })
    }
}

/// Holds the sweep lock. Releasing it wakes waiters, so a caller that found
/// the lock taken can sleep on the condvar until this sweep returns.
struct SweepBusy<'a> {
    wake: DiskWake,
    guard: Option<MutexGuard<'a, ()>>,
}

impl Drop for SweepBusy<'_> {
    fn drop(&mut self) {
        self.guard.take();
        self.wake.notify();
    }
}

impl Drop for WriteHold<'_> {
    fn drop(&mut self) {
        let _guard = self.gate.lock_wake();
        self.gate.write_holds.fetch_sub(1, Ordering::SeqCst);
        self.gate.wake.notify_locked();
    }
}

impl Drop for RecallHold<'_> {
    fn drop(&mut self) {
        let _guard = self.gate.lock_wake();
        self.gate.recall_waiters.fetch_sub(1, Ordering::SeqCst);
        self.gate.wake.notify_locked();
    }
}

/// Clears the owed recall flush on every exit, including a write error.
struct ClearRecallFlush<'a> {
    disk: &'a DiskGate,
}

impl Drop for ClearRecallFlush<'_> {
    fn drop(&mut self) {
        self.disk.clear_recall_flush();
    }
}

impl StorageModule {
    /// Mining recall. The caller announces before `chunks.dat` is locked, so a
    /// sweep or write that has not taken the file yet lets this read run first.
    ///
    /// The `pread`s stay on this caller and start while packed writes already
    /// in the kernel are still running. Two slices of the range are in the
    /// kernel together. While this recall holds the disk the lane submits no
    /// new writes. When the hold drops, the lane resumes windows of packed
    /// chunks.
    pub fn read_recall_chunks(
        &self,
        chunk_range: Interval<PartitionChunkOffset>,
    ) -> eyre::Result<super::ChunkMap> {
        let chunks = {
            let _recall = self.disk.begin_recall();
            self.read_chunks_inner(chunk_range, true)?
        };
        if let Err(error) = self.flush_after_recall() {
            tracing::error!(
                "Couldn't flush packed chunks after recall for storage_module {}: {error}",
                self.id
            );
        }
        // Entropy stays behind the packed window. The lane thread sweeps
        // itself; this caller only does it when that thread is not running.
        if !self.disk.lane_running() {
            self.pump_entropy_reads();
        }
        Ok(chunks)
    }

    /// Without a lane thread, commit one window before the caller returns.
    /// The lane thread does this itself: it keeps a window in flight and
    /// stops submitting while a recall holds the disk.
    fn flush_after_recall(&self) -> eyre::Result<()> {
        if self.disk.lane_running() {
            return Ok(());
        }
        if !self.has_pending_writes() {
            return Ok(());
        }
        self.disk.arm_recall_flush();
        match self.sync_in_progress.try_lock() {
            Ok(guard) => drop(guard),
            Err(TryLockError::WouldBlock) => {
                self.disk.clear_recall_flush();
                return Ok(());
            }
            Err(TryLockError::Poisoned(poisoned)) => drop(poisoned.into_inner()),
        }
        self.flush_packed_window()
    }

    fn flush_packed_window(&self) -> eyre::Result<()> {
        let _clear = ClearRecallFlush { disk: &self.disk };
        // No-lane callers flush one small window. The lane passes INFLIGHT_WRITES.
        self.commit_pending_runs(false, Some(INFLIGHT_DISK_OPS), false)
    }

    /// One entropy sweep when the disk is free, plus entropy already in memory.
    /// The service tick calls this after the packed-write flush.
    pub fn pump_entropy_reads(&self) {
        self.poll_acks();
        if self.disk.recall_pending() || self.disk.writes_active() {
            return;
        }
        self.fold_memory_entropy();
        if self.disk.lane_running() {
            self.sweep_inflight();
        } else {
            self.sweep_one();
        }
        self.poll_acks();
    }

    /// Occupy entropy targets and return. The sweep packs them later.
    /// Unpacked bytes count toward `pending_write_bytes`.
    pub fn deposit_data_chunk(&self, chunk: &UnpackedChunk) -> Result<(), WriteDataChunkError> {
        self.enqueue_unpacked(chunk, WritePriority::Migration, false)
            .map(|_| ())
    }

    /// Publish every body in `chunks` before waking the lane once.
    /// Migration uses this so a contiguous run is one entropy span.
    pub fn deposit_data_chunks(
        &self,
        chunks: &[UnpackedChunk],
    ) -> Result<Vec<BatchEnqueueItem>, WriteDataChunkError> {
        self.enqueue_unpacked_batch(chunks, WritePriority::Migration)
    }

    /// Same publish as [`Self::deposit_data_chunks`], at ingress priority.
    /// Data sync uses this for one fetched contiguous run.
    pub fn enqueue_ingress_batch(
        &self,
        chunks: &[UnpackedChunk],
    ) -> Result<Vec<BatchEnqueueItem>, WriteDataChunkError> {
        self.enqueue_unpacked_batch(chunks, WritePriority::Ingress)
    }

    pub(super) fn enqueue_unpacked(
        &self,
        chunk: &UnpackedChunk,
        priority: WritePriority,
        waited: bool,
    ) -> Result<Option<u64>, WriteDataChunkError> {
        self.enqueue_unpacked_with(chunk, priority, waited, None)
    }

    pub(super) fn enqueue_unpacked_with(
        &self,
        chunk: &UnpackedChunk,
        priority: WritePriority,
        waited: bool,
        notify: Option<SweepNotify>,
    ) -> Result<Option<u64>, WriteDataChunkError> {
        match self.occupy_unpacked(chunk, priority, waited, notify) {
            Ok(Some(occupied)) => {
                let groups = self.push_occupied(vec![occupied])?;
                Ok(groups.into_iter().next())
            }
            Ok(None) => Ok(None),
            Err(OccupyFailure::InFlight) => Err(WriteDataChunkError::Other(eyre::eyre!(
                "index write already in flight"
            ))),
            Err(OccupyFailure::Write(error)) => Err(error),
        }
    }

    /// Occupy every body, then publish the occupied slots under one queue lock.
    fn enqueue_unpacked_batch(
        &self,
        chunks: &[UnpackedChunk],
        priority: WritePriority,
    ) -> Result<Vec<BatchEnqueueItem>, WriteDataChunkError> {
        let mut held: Vec<OccupiedChunk> = Vec::new();
        let mut items = Vec::with_capacity(chunks.len());
        for chunk in chunks {
            match self.occupy_unpacked(chunk, priority, false, None) {
                Ok(Some(occupied)) => {
                    let offsets = occupied
                        .selected
                        .iter()
                        .map(|(offset, _, _)| *offset)
                        .collect();
                    held.push(occupied);
                    items.push(BatchEnqueueItem::Queued(offsets));
                }
                Ok(None) => items.push(BatchEnqueueItem::NotQueued),
                Err(OccupyFailure::InFlight) => items.push(BatchEnqueueItem::NotQueued),
                Err(OccupyFailure::Write(WriteDataChunkError::DataRootNotFound)) => {
                    items.push(BatchEnqueueItem::DataRootNotFound);
                }
                Err(OccupyFailure::Write(WriteDataChunkError::WritesPaused)) => {
                    self.release_occupied(held);
                    return Err(WriteDataChunkError::WritesPaused);
                }
                Err(OccupyFailure::Write(error)) => {
                    self.release_occupied(held);
                    return Err(error);
                }
            }
        }
        if !held.is_empty() {
            self.push_occupied(held)?;
        }
        Ok(items)
    }

    /// Reserve entropy targets for one body. The sweep slot is not visible yet.
    fn occupy_unpacked(
        &self,
        chunk: &UnpackedChunk,
        priority: WritePriority,
        waited: bool,
        notify: Option<SweepNotify>,
    ) -> Result<Option<OccupiedChunk>, OccupyFailure> {
        if self.data_writes_paused() {
            return Err(OccupyFailure::Write(WriteDataChunkError::WritesPaused));
        }
        let partition_offsets = match super::index_read_metrics::trace_index_read(
            super::index_read_metrics::PLACEMENT,
            || self.partition_offsets_for_data_root_chunk(chunk.data_root, chunk.tx_offset),
        ) {
            Ok(Some(offsets)) => offsets,
            Ok(None) => {
                return Err(OccupyFailure::Write(WriteDataChunkError::DataRootNotFound));
            }
            Err(error) => return Err(OccupyFailure::Write(error.into())),
        };

        let data_path = Arc::new(chunk.data_path.0.clone());
        let path_hash = UnpackedChunk::hash_data_path(&data_path);
        let unpacked = Arc::new(chunk.bytes.0.clone());

        let disk_entropy: std::collections::HashSet<PartitionChunkOffset> = {
            let intervals = self.intervals.read().unwrap();
            partition_offsets
                .iter()
                .copied()
                .filter(|offset| {
                    intervals
                        .get_at_point(*offset)
                        .is_some_and(|ty| *ty == ChunkType::Entropy)
                })
                .collect()
        };

        let mut selected: Vec<(PartitionChunkOffset, u64, Option<Vec<u8>>)> = Vec::new();
        {
            let mut pending = self.pending_writes.write().unwrap();
            if self.data_writes_paused() {
                return Err(OccupyFailure::Write(WriteDataChunkError::WritesPaused));
            }
            let generation = self.index_write_generation.load(Ordering::SeqCst);
            for partition_offset in &partition_offsets {
                let partition_offset = *partition_offset;
                if pending.occupancy.contains_key(&partition_offset)
                    || pending
                        .get(&partition_offset)
                        .is_some_and(|(_, chunk_type)| *chunk_type == ChunkType::Data)
                {
                    continue;
                }
                let pending_entropy = pending
                    .get(&partition_offset)
                    .and_then(|(bytes, ty)| (*ty == ChunkType::Entropy).then(|| bytes.clone()));
                if pending_entropy.is_none() && !disk_entropy.contains(&partition_offset) {
                    continue;
                }
                pending.occupancy.insert(partition_offset, generation);
                selected.push((partition_offset, generation, pending_entropy));
            }
            if selected.is_empty() {
                if partition_offsets
                    .iter()
                    .any(|offset| pending.occupancy.contains_key(offset))
                {
                    return Err(OccupyFailure::InFlight);
                }
                return Ok(None);
            }
            let queued = unpacked.len() as u64 * selected.len() as u64;
            pending.queued_unpacked_bytes = pending.queued_unpacked_bytes.saturating_add(queued);
        }

        Ok(Some(OccupiedChunk {
            unpacked,
            data_path,
            path_hash,
            priority,
            waited,
            notify,
            selected,
        }))
    }

    /// Make every occupied body visible, then wake the lane once.
    fn push_occupied(&self, occupied: Vec<OccupiedChunk>) -> Result<Vec<u64>, WriteDataChunkError> {
        if occupied.is_empty() {
            return Ok(Vec::new());
        }
        let mut queue = self.disk.queue();
        // A pause can land after occupancy is taken and before the slot is visible.
        let generation_now = self.index_write_generation.load(Ordering::SeqCst);
        let stale = self.data_writes_paused()
            || occupied.iter().any(|chunk| {
                chunk
                    .selected
                    .iter()
                    .any(|(_, generation, _)| *generation != generation_now)
            });
        if stale {
            drop(queue);
            self.release_occupied(occupied);
            return Err(WriteDataChunkError::WritesPaused);
        }
        let queued_at = Instant::now();
        let mut groups = Vec::with_capacity(occupied.len());
        for chunk in occupied {
            let group_id = queue.next_group;
            queue.next_group = queue.next_group.wrapping_add(1);
            let byte_len = chunk.unpacked.len() as u64;
            queue.groups.insert(
                group_id,
                SweepGroup {
                    remaining: chunk.selected.len(),
                    failed: None,
                    waited: chunk.waited,
                    notify: chunk.notify,
                },
            );
            for (offset, generation, pending_entropy) in chunk.selected {
                queue.slots.push(SweepSlot {
                    group: group_id,
                    offset,
                    unpacked: Arc::clone(&chunk.unpacked),
                    data_path: Arc::clone(&chunk.data_path),
                    path_hash: chunk.path_hash,
                    generation,
                    priority: chunk.priority,
                    pending_entropy,
                    byte_len,
                    queued_at,
                });
            }
            groups.push(group_id);
        }
        drop(queue);
        #[cfg(test)]
        {
            self.disk.enqueue_notifies.fetch_add(1, Ordering::SeqCst);
        }
        self.disk.notify();
        Ok(groups)
    }

    fn release_occupied(&self, occupied: impl IntoIterator<Item = OccupiedChunk>) {
        for chunk in occupied {
            let byte_len = chunk.unpacked.len() as u64;
            for (offset, generation, _) in chunk.selected {
                self.release_queued_offset(offset, generation, byte_len);
            }
        }
    }

    pub(super) fn drive_group(&self, group: u64) -> Result<(), WriteDataChunkError> {
        let mut deadline = Instant::now() + SWEEP_WAIT;
        loop {
            self.poll_acks();
            if let Some(result) = self.take_group_result(group) {
                return result;
            }
            // Recall can stay queued for the whole step. Stop this waiter on
            // the wall clock so the wait does not run until recall goes idle.
            // A group that finished during the wait keeps that result.
            if Instant::now() >= deadline || !self.disk.yield_to_recall_until(deadline) {
                self.poll_acks();
                if let Some(result) = self.take_group_result(group) {
                    return result;
                }
                if let Some(result) = self.abandon_wait(group) {
                    return result;
                }
                return Err(entropy_read_unfinished());
            }
            if self.disk.writes_active() {
                self.disk.yield_to_writes();
                deadline = Instant::now() + SWEEP_WAIT;
                continue;
            }
            let folded = self.fold_memory_entropy();
            let swept = self.sweep_one();
            if folded || swept {
                deadline = Instant::now() + SWEEP_WAIT;
                continue;
            }
            // The attempt above already woke the condvar when it released the
            // sweep lock. Read the epoch after that, then collect an ack that
            // landed during the attempt, then sleep until a later wake.
            // A short span waits out `reorder_grace` while the disk is busy.
            // Wake then, so this caller retries when the span becomes eligible.
            // An idle disk reads the span on the attempt above. The sweep
            // give-up still bounds the wait.
            let seen = self.disk.wake.epoch();
            self.poll_acks();
            if let Some(result) = self.take_group_result(group) {
                return result;
            }
            let wake_by = self
                .read_grace_remaining()
                .filter(|delay| !delay.is_zero())
                .and_then(|delay| Instant::now().checked_add(delay))
                .map(|at| at.min(deadline))
                .unwrap_or(deadline);
            self.wait_until_epoch_advances(seen, wake_by);
        }
    }

    /// Wait until the lane thread finishes `group`. Does not lock `chunks.dat`.
    /// The result check holds the wake lock, so a completion notify cannot
    /// land in the gap before the wait. The bound is the sweep give-up.
    pub(super) fn wait_group(&self, group: u64) -> Result<(), WriteDataChunkError> {
        let deadline = Instant::now() + SWEEP_WAIT;
        loop {
            let Some(remain) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            if remain.is_zero() {
                break;
            }
            let guard = self.disk.lock_wake();
            if let Some(result) = self.take_group_result(group) {
                return result;
            }
            let _guard = self.disk.wake.wait_timeout(guard, remain);
        }
        if let Some(result) = self.abandon_wait(group) {
            return result;
        }
        Err(entropy_read_unfinished())
    }

    pub(super) async fn await_swept_group(
        &self,
        group: u64,
        mut rx: tokio::sync::oneshot::Receiver<Result<(), WriteDataChunkError>>,
    ) -> Result<(), WriteDataChunkError> {
        // `timeout` drops its future on expiry. Poll the receiver in place so
        // a result sent as the deadline passes is still available to try_recv.
        let wait =
            std::future::poll_fn(|cx| std::future::Future::poll(std::pin::Pin::new(&mut rx), cx));
        match tokio::time::timeout(SWEEP_WAIT, wait).await {
            Ok(Ok(result)) => result,
            Ok(Err(_closed)) => Err(entropy_read_unfinished()),
            Err(_elapsed) => {
                if let Some(result) = self.abandon_wait(group) {
                    return result;
                }
                match rx.try_recv() {
                    Ok(result) => result,
                    Err(_empty_or_closed) => Err(entropy_read_unfinished()),
                }
            }
        }
    }

    /// Pack every queued entropy read. Force-flush and shutdown use this so a
    /// deposit reaches the write queue. A stretch with no completion still
    /// ends at `SWEEP_WAIT`.
    pub(super) fn drain_entropy_queue(&self) {
        let mut idle_deadline: Option<Instant> = None;
        loop {
            self.poll_acks();
            if self.entropy_idle() {
                return;
            }
            self.disk.yield_to_recall();
            let folded = self.fold_memory_entropy();
            let swept = self.sweep_one_with(true);
            if folded || swept {
                idle_deadline = None;
                continue;
            }
            let seen = self.disk.wake.epoch();
            self.poll_acks();
            if self.entropy_idle() {
                return;
            }
            let deadline = idle_deadline.get_or_insert_with(|| Instant::now() + SWEEP_WAIT);
            if self.wait_until_epoch_advances(seen, *deadline) {
                tracing::warn!("entropy queue did not drain");
                return;
            }
        }
    }

    /// Sleep until `epoch` moves past `seen`, or until `deadline`.
    /// Returns true when the deadline passes first. The check and the wait
    /// share the wake lock, so a completion cannot land between them.
    fn wait_until_epoch_advances(&self, seen: u64, deadline: Instant) -> bool {
        let mut guard = self.disk.lock_wake();
        loop {
            if self.disk.wake.epoch() != seen {
                return false;
            }
            let Some(remain) = deadline.checked_duration_since(Instant::now()) else {
                return true;
            };
            if remain.is_zero() {
                return true;
            }
            guard = self.disk.wake.wait_timeout(guard, remain);
        }
    }

    pub(super) fn fail_queued_sweeps(&self) {
        self.fail_slots(|_| true, &WriteDataChunkError::WritesPaused);
    }

    pub(super) fn cancel_sweep_range(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) {
        let groups: Vec<u64> = {
            let queue = self.disk.queue();
            queue
                .slots
                .iter()
                .filter(|slot| slot.offset >= start && slot.offset <= end)
                .map(|slot| slot.group)
                .collect()
        };
        if groups.is_empty() {
            return;
        }
        self.fail_slots(
            |slot| groups.contains(&slot.group),
            &WriteDataChunkError::WritesPaused,
        );
    }

    fn entropy_idle(&self) -> bool {
        let queue = self.disk.queue();
        queue.slots.is_empty() && queue.inflight.is_empty() && queue.releases.is_empty()
    }

    /// Drop the waiter. Slots already queued still pack. A group that already
    /// finished is returned so the caller does not lose that result.
    fn abandon_wait(&self, group: u64) -> Option<Result<(), WriteDataChunkError>> {
        let mut queue = self.disk.queue();
        let entry = queue.groups.get_mut(&group)?;
        entry.waited = false;
        if entry.remaining != 0 || entry.notify.is_some() {
            return None;
        }
        let entry = queue.groups.remove(&group)?;
        Some(match entry.failed {
            Some(error) => Err(error),
            None => Ok(()),
        })
    }

    fn take_group_result(&self, group: u64) -> Option<Result<(), WriteDataChunkError>> {
        let mut queue = self.disk.queue();
        let done = queue
            .groups
            .get(&group)
            .is_some_and(|entry| entry.remaining == 0);
        if !done {
            return None;
        }
        let entry = queue.groups.remove(&group)?;
        Some(match entry.failed {
            Some(error) => Err(error),
            None => Ok(()),
        })
    }

    /// XOR entropy that is already buffered. No `chunks.dat` read.
    /// Only the best priority still queued is packed, and only when that
    /// priority already has buffered entropy. A disk slot of a better priority
    /// keeps the file read ahead of a lower-priority memory fold.
    fn fold_memory_entropy(&self) -> bool {
        let Some(_busy) = self.disk.try_sweep_busy() else {
            return false;
        };
        let ready = {
            let mut queue = self.disk.queue();
            let Some(best) = queue.slots.iter().map(|slot| slot.priority).min() else {
                return false;
            };
            if !queue
                .slots
                .iter()
                .any(|slot| slot.priority == best && slot.pending_entropy.is_some())
            {
                return false;
            }
            let mut ready = Vec::new();
            let mut keep = Vec::new();
            for slot in queue.slots.drain(..) {
                if slot.pending_entropy.is_some() && slot.priority == best {
                    ready.push(slot);
                } else {
                    keep.push(slot);
                }
            }
            queue.slots = keep;
            ready
        };
        if ready.is_empty() {
            return false;
        }
        let mut deferred = Vec::new();
        let mut packed_any = false;
        for slot in ready {
            if self.disk.recall_pending() {
                deferred.push(slot);
                continue;
            }
            self.pack_one(slot);
            packed_any = true;
        }
        if !deferred.is_empty() {
            self.disk.queue().slots.extend(deferred);
        }
        packed_any
    }

    /// One pread, bounded by the sweep cap. Bytes that cover a hole are discarded.
    /// Index submits happen only after every kept offset in the span has been
    /// rechecked, so one bad offset does not leave a sibling committed.
    /// A normal call holds a short span. `force` reads it during a drain.
    fn sweep_one(&self) -> bool {
        self.sweep_one_with(false)
    }

    fn sweep_one_with(&self, force: bool) -> bool {
        let Some(_busy) = self.disk.try_sweep_busy() else {
            return false;
        };
        if self.disk.recall_pending() || self.disk.writes_active() {
            return false;
        }
        let chunk_size = self.config.consensus.chunk_size.max(1);
        // Capture once. An idle disk releases every short span and must not
        // arm the hold. A later submit in this call must not hide the rest.
        let due = self.reads_released(self.queued_disk_slots());
        let Some(span) = self.take_ready_span(force, true, due) else {
            return false;
        };
        let short = !force && !due && pread_span_bytes(&span, chunk_size) < self.hold_cap_bytes();
        if self.disk.recall_pending() || self.disk.writes_active() {
            self.restore_span(span);
            return false;
        }

        let start = span[0].offset;
        let end = span[span.len() - 1].offset;
        let chunk_len = chunk_size as usize;
        let span_chunks = (end.0 - start.0 + 1) as usize;
        let (file_arc, file_offset) = {
            let Ok((interval, submodule)) = self.submodules.get_key_value_at_point(start) else {
                let error = WriteDataChunkError::Other(eyre::eyre!(
                    "No submodule found for Partition Offset {start:?}"
                ));
                self.fail_span(span, &error);
                return true;
            };
            let submodule_offset = start - interval.start();
            (
                Arc::clone(&submodule.file),
                u64::from(submodule_offset) * chunk_size,
            )
        };
        let mut buf = vec![0_u8; span_chunks.saturating_mul(chunk_len)];
        // A recall or flush that arrived after the span was taken goes
        // first. Put the span back and let the caller wait, so this sweep
        // does not sit inside a recall that never goes idle.
        if self.disk.writes_active() || self.disk.recall_pending() {
            self.restore_span(span);
            return false;
        }
        let file = self.disk.lock_chunks(&file_arc);
        if self.disk.recall_pending() || self.disk.writes_active() {
            drop(file);
            self.restore_span(span);
            return false;
        }
        if let Err(error) = file.read_exact_at(&mut buf, file_offset) {
            drop(file);
            let error = WriteDataChunkError::Other(eyre::eyre!(
                "entropy sweep at offset {start} count {span_chunks}: {error}"
            ));
            self.fail_span(span, &error);
            return true;
        }
        drop(file);
        #[cfg(test)]
        self.disk.entropy_preads.fetch_add(1, Ordering::SeqCst);
        self.finish_span_read(span, start, &buf);
        if short {
            self.disk.arm_read_short_hold(reorder_grace(chunk_size));
        }
        true
    }

    /// Entropy preads up to the service budget. A busy pass reads every full
    /// span and one short span, then arms the read hold. An idle pass reads
    /// every span that fits and does not arm the hold. The lane thread XORs
    /// and submits the index only after those calls return.
    fn sweep_inflight(&self) {
        if self.disk.recall_pending() || self.disk.writes_active() {
            return;
        }
        let chunk_size = self.config.consensus.chunk_size.max(1);
        let cap = self.hold_cap_bytes();
        // Capture once. The first submit makes the disk busy. The rest of
        // this pass still uses the idle release it started with.
        let due = self.reads_released(self.queued_disk_slots());
        let mut allow_short = true;
        let mut spans = Vec::new();
        let mut short_marks = Vec::new();
        let mut guards = Vec::with_capacity(INFLIGHT_WRITES);
        while spans.len() < INFLIGHT_WRITES {
            if self.disk.recall_pending() || self.disk.writes_active() {
                break;
            }
            let Some(span) = self.take_ready_span(false, allow_short, due) else {
                break;
            };
            let bytes = pread_span_bytes(&span, chunk_size);
            if !self.disk.try_submit_read(INFLIGHT_WRITES, bytes) {
                self.restore_span(span);
                break;
            }
            let short = !due && bytes < cap;
            if short {
                allow_short = false;
            }
            spans.push(span);
            short_marks.push(short);
            guards.push(OpGuard {
                disk: &self.disk,
                bytes,
            });
        }
        if spans.is_empty() {
            return;
        }
        let results = std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(spans.len());
            for (span, guard) in spans.iter().zip(guards) {
                handles.push(scope.spawn(move || {
                    let _guard = guard;
                    self.read_span_inflight(span)
                }));
            }
            handles
                .into_iter()
                .map(|handle| handle.join().expect("entropy pread"))
                .collect::<Vec<_>>()
        });
        let mut read_short = false;
        for ((span, short), result) in spans.into_iter().zip(short_marks).zip(results) {
            match result {
                Ok(load) => {
                    read_short |= short;
                    self.finish_span_read(span, load.start, &load.buf);
                }
                Err(error) => self.fail_span(span, &error),
            }
        }
        if read_short {
            self.disk.arm_read_short_hold(reorder_grace(chunk_size));
        }
    }

    fn read_span_inflight(&self, span: &[SweepSlot]) -> Result<SpanLoad, WriteDataChunkError> {
        let start = span[0].offset;
        let end = span[span.len() - 1].offset;
        let chunk_size = self.config.consensus.chunk_size.max(1);
        let chunk_len = chunk_size as usize;
        let span_chunks = (end.0 - start.0 + 1) as usize;
        let (file, file_offset) = {
            let Ok((interval, submodule)) = self.submodules.get_key_value_at_point(start) else {
                return Err(WriteDataChunkError::Other(eyre::eyre!(
                    "No submodule found for Partition Offset {start:?}"
                )));
            };
            let submodule_offset = start - interval.start();
            (
                Arc::clone(&submodule.file),
                u64::from(submodule_offset) * chunk_size,
            )
        };
        let mut buf = vec![0_u8; span_chunks.saturating_mul(chunk_len)];
        let cloned = {
            let guard = file.lock().unwrap_or_else(PoisonError::into_inner);
            guard.try_clone().map_err(|error| {
                WriteDataChunkError::Other(eyre::eyre!(
                    "entropy sweep at offset {start} count {span_chunks}: {error}"
                ))
            })?
        };
        cloned
            .read_exact_at(&mut buf, file_offset)
            .map_err(|error| {
                WriteDataChunkError::Other(eyre::eyre!(
                    "entropy sweep at offset {start} count {span_chunks}: {error}"
                ))
            })?;
        #[cfg(test)]
        self.disk.entropy_preads.fetch_add(1, Ordering::SeqCst);
        Ok(SpanLoad { start, buf })
    }

    fn finish_span_read(&self, span: Vec<SweepSlot>, start: PartitionChunkOffset, buf: &[u8]) {
        let chunk_len = self.config.consensus.chunk_size.max(1) as usize;
        let mut failed_groups = std::collections::HashSet::new();
        let mut accepted = Vec::new();
        for slot in span {
            let group = slot.group;
            if failed_groups.contains(&group) {
                self.fail_slot(slot, WriteDataChunkError::WritesPaused);
                continue;
            }
            if let Some(error) = self.injected_entropy_read_error() {
                failed_groups.insert(group);
                self.fail_slot(slot, error);
                self.fail_queued_group(group);
                continue;
            }
            if let Err(error) = self.slot_still_valid(&slot) {
                failed_groups.insert(group);
                self.fail_slot(slot, error);
                self.fail_queued_group(group);
                continue;
            }
            let start_index = (slot.offset.0 - start.0) as usize * chunk_len;
            let entropy = buf[start_index..start_index + chunk_len].to_vec();
            accepted.push((slot, entropy));
        }
        let mut pending_submit = Vec::with_capacity(accepted.len());
        for (slot, entropy) in accepted {
            if failed_groups.contains(&slot.group) {
                let group = slot.group;
                self.fail_slot(slot, WriteDataChunkError::WritesPaused);
                self.fail_queued_group(group);
                continue;
            }
            pending_submit.push((slot, entropy));
        }
        self.submit_contiguous_islands(pending_submit);
    }

    /// Split kept offsets into islands. A hole is not a member: only the next
    /// offset joins. An island longer than one chunk is inserted as one step
    /// after every index ack, so a write pass cannot see a prefix of it.
    fn submit_contiguous_islands(&self, accepted: Vec<(SweepSlot, Vec<u8>)>) {
        let mut island: Vec<(SweepSlot, Vec<u8>)> = Vec::new();
        for (slot, entropy) in accepted {
            let joins = island
                .last()
                .is_some_and(|(prev, _)| prev.offset.0.checked_add(1) == Some(slot.offset.0));
            if !island.is_empty() && !joins {
                self.submit_island(std::mem::take(&mut island));
            }
            island.push((slot, entropy));
        }
        if !island.is_empty() {
            self.submit_island(island);
        }
    }

    fn submit_island(&self, island: Vec<(SweepSlot, Vec<u8>)>) {
        let release_id = (island.len() > 1).then(|| self.open_span_release(island.len()));
        for (slot, entropy) in island {
            self.submit_packed(slot, entropy, release_id);
        }
    }

    fn open_span_release(&self, len: usize) -> u64 {
        let mut queue = self.disk.queue();
        let id = queue.next_release;
        queue.next_release = queue.next_release.wrapping_add(1);
        queue.releases.insert(
            id,
            SpanRelease {
                left: len,
                ready: Vec::with_capacity(len),
            },
        );
        id
    }

    fn restore_span(&self, span: Vec<SweepSlot>) {
        self.disk.queue().slots.extend(span);
    }

    fn fail_span(&self, span: Vec<SweepSlot>, error: &WriteDataChunkError) {
        for slot in span {
            let group = slot.group;
            self.fail_slot(slot, clone_error(error));
            self.fail_queued_group(group);
        }
    }

    fn take_disk_span(&self) -> Option<Vec<SweepSlot>> {
        let due = self.reads_released(self.queued_disk_slots());
        self.take_ready_span(false, true, due)
    }

    /// Remove the first span `ready_disk_spans` would issue. `due` is the
    /// pass-wide count, so taking one span does not hide the rest of a full queue.
    fn take_ready_span(&self, force: bool, allow_short: bool, due: bool) -> Option<Vec<SweepSlot>> {
        let mut queue = self.disk.queue();
        let chosen = self
            .ready_disk_spans(&queue.slots, force, allow_short, due)
            .into_iter()
            .next()?;
        let mut indexes = chosen.indexes;
        indexes.sort_unstable();
        indexes.dedup();
        let mut span = Vec::with_capacity(indexes.len());
        for index in indexes.into_iter().rev() {
            span.push(queue.slots.remove(index));
        }
        // Removal walks queue indexes. The read treats the first slot as the
        // low offset and the last as the high one, so the span has to leave
        // in offset order.
        span.sort_by_key(|slot| slot.offset);
        Some(span)
    }

    /// Bytes of the longest entropy `pread` that may hit the disk now.
    /// While the disk is busy, a young or held short span stays out of this
    /// count, so a packed write can use the disk while that span waits.
    /// An idle disk counts the short span, so the longer command still wins.
    fn longest_disk_span_bytes(&self) -> u64 {
        let queue = self.disk.queue();
        let due = self.reads_released(disk_slot_count(&queue.slots));
        self.ready_disk_spans(&queue.slots, false, true, due)
            .into_iter()
            .map(|span| span.bytes)
            .max()
            .unwrap_or(0)
    }

    fn has_ready_disk_span(&self) -> bool {
        let queue = self.disk.queue();
        let due = self.reads_released(disk_slot_count(&queue.slots));
        !self
            .ready_disk_spans(&queue.slots, false, true, due)
            .is_empty()
    }

    /// Disk slots in the sweep queue. Memory entropy does not count.
    fn queued_disk_slots(&self) -> usize {
        disk_slot_count(&self.disk.queue().slots)
    }

    /// The durability count and an owed recall flush read every short span.
    fn disk_reads_due(&self, disk_slots: usize) -> bool {
        let threshold = self.config.node_config.storage.num_writes_before_sync;
        self.disk.recall_flush_is_owed() || disk_slots as u64 >= threshold
    }

    /// Every short span is eligible. The durability count, an owed recall
    /// flush, and an idle disk all set this. The caller passes it as `due`,
    /// so the pass ranks the spans normally and does not arm the read hold.
    fn reads_released(&self, disk_slots: usize) -> bool {
        self.disk_reads_due(disk_slots) || self.disk.chunk_disk_idle()
    }

    /// One span per contiguous neighborhood. The next span starts after the
    /// previous one ends, so two spans in one file do not share a slot.
    fn maximal_disk_spans(&self, slots: &[SweepSlot]) -> Vec<DiskSpan> {
        let cap = self.hold_cap_bytes();
        let hole_bytes = self.config.node_config.storage.entropy_coalesce_hole_bytes;
        let chunk_size = self.config.consensus.chunk_size.max(1);
        let grace = reorder_grace(chunk_size);
        let mut groups: HashMap<i64, Vec<SpanAnchor>> = HashMap::new();
        for (index, slot) in slots.iter().enumerate() {
            if slot.pending_entropy.is_some() {
                continue;
            }
            let lone = i64::try_from(index)
                .map(|value| value.saturating_add(1))
                .unwrap_or(i64::MAX);
            let key = self
                .submodules
                .get_key_value_at_point(slot.offset)
                .ok()
                .map(|(interval, _)| i64::from(interval.start().0))
                .unwrap_or(-lone);
            groups.entry(key).or_default().push(SpanAnchor {
                index,
                offset: slot.offset.0,
                queued_at: slot.queued_at,
                priority: slot.priority,
            });
        }
        let mut spans = Vec::new();
        for mut anchors in groups.into_values() {
            anchors.sort_by_key(|anchor| anchor.offset);
            spans.extend(partition_file_spans(
                &anchors, chunk_size, hole_bytes, cap, grace,
            ));
        }
        spans
    }

    /// Spans this pass may read. Every full span is included. While the disk
    /// is busy, one aged short span is included, the oldest, and it is issued
    /// first. `force` or `due` includes every short span and does not pull one
    /// to the front. The caller sets `due` for the durability count, an owed
    /// recall flush, and an idle disk.
    fn ready_disk_spans(
        &self,
        slots: &[SweepSlot],
        force: bool,
        allow_short: bool,
        due: bool,
    ) -> Vec<DiskSpan> {
        let cap = self.hold_cap_bytes();
        let held = !force && self.disk.short_reads_held();
        let mut ready = Vec::new();
        let mut short: Option<DiskSpan> = None;
        for span in self.maximal_disk_spans(slots) {
            if force || due || span.bytes >= cap {
                ready.push(span);
                continue;
            }
            if !allow_short || held || !span.aged {
                continue;
            }
            let take = match &short {
                None => true,
                Some(prev) => {
                    span.oldest < prev.oldest
                        || (span.oldest == prev.oldest && span.start < prev.start)
                }
            };
            if take {
                short = Some(span);
            }
        }
        ready.sort_by(|left, right| {
            right
                .aged
                .cmp(&left.aged)
                .then(right.bytes.cmp(&left.bytes))
                .then(left.oldest.cmp(&right.oldest))
                .then(left.priority.cmp(&right.priority))
                .then(left.start.cmp(&right.start))
        });
        if let Some(short) = short {
            ready.insert(0, short);
        }
        ready
    }

    fn pack_one(&self, slot: SweepSlot) {
        let group = slot.group;
        if let Err(error) = self.slot_still_valid(&slot) {
            self.fail_slot(slot, error);
            self.fail_queued_group(group);
            return;
        }
        let Some(entropy) = slot.pending_entropy.clone() else {
            self.fail_slot(
                slot,
                WriteDataChunkError::Other(eyre::eyre!("missing buffered entropy")),
            );
            self.fail_queued_group(group);
            return;
        };
        self.submit_packed(slot, entropy, None);
    }

    fn submit_packed(&self, slot: SweepSlot, entropy: Vec<u8>, release_id: Option<u64>) {
        let group = slot.group;
        let group_failed = self
            .disk
            .queue()
            .groups
            .get(&group)
            .is_some_and(|entry| entry.failed.is_some());
        if group_failed {
            self.fail_slot(slot, WriteDataChunkError::WritesPaused);
            self.note_release_gap(release_id);
            return;
        }
        // Recheck immediately before the index submit. A pause or reset that
        // landed after the read must not commit this offset.
        if let Err(error) = self.slot_still_valid(&slot) {
            self.fail_slot(slot, error);
            self.fail_queued_group(group);
            self.note_release_gap(release_id);
            return;
        }
        let packed = packing_xor_vec_u8(entropy, &slot.unpacked);
        let (done_tx, done_rx) = mpsc::channel();
        let offset = slot.offset;
        let Some((_, submodule)) = self.submodules.get_key_value_at_point(offset).ok() else {
            self.fail_slot(
                slot,
                WriteDataChunkError::Other(eyre::eyre!(
                    "No submodule found for Partition Offset {offset:?}"
                )),
            );
            self.fail_queued_group(group);
            self.note_release_gap(release_id);
            return;
        };
        submodule.index_drain.submit(IndexOp {
            path_hash: slot.path_hash,
            data_path: (*slot.data_path).clone(),
            offset: slot.offset,
            generation: slot.generation,
            done: done_tx,
            wake: Some(self.disk.wake.clone()),
        });
        self.disk.queue().inflight.push(Inflight {
            group: slot.group,
            offset: slot.offset,
            packed,
            generation: slot.generation,
            priority: slot.priority,
            byte_len: slot.byte_len,
            release_id,
            done: done_rx,
        });
    }

    fn slot_still_valid(&self, slot: &SweepSlot) -> Result<(), WriteDataChunkError> {
        if self.data_writes_paused()
            || self.index_write_generation.load(Ordering::SeqCst) != slot.generation
        {
            return Err(WriteDataChunkError::WritesPaused);
        }
        let occupied = {
            let pending = self.pending_writes.read().unwrap();
            pending.occupancy.get(&slot.offset).copied() == Some(slot.generation)
        };
        if !occupied {
            return Err(WriteDataChunkError::WritesPaused);
        }
        if slot.pending_entropy.is_some() {
            return Ok(());
        }
        let intervals = self.intervals.read().unwrap();
        if intervals
            .get_at_point(slot.offset)
            .is_some_and(|ty| *ty == ChunkType::Entropy)
        {
            Ok(())
        } else {
            Err(WriteDataChunkError::Other(eyre::eyre!(
                "entropy interval changed before pack"
            )))
        }
    }

    fn fail_slot(&self, slot: SweepSlot, error: WriteDataChunkError) {
        self.release_queued_offset(slot.offset, slot.generation, slot.byte_len);
        self.mark_group_slot_done(slot.group, Some(error));
    }

    fn fail_queued_group(&self, group: u64) {
        self.fail_slots(
            |slot| slot.group == group,
            &WriteDataChunkError::WritesPaused,
        );
    }

    fn fail_slots(&self, mut pred: impl FnMut(&SweepSlot) -> bool, error: &WriteDataChunkError) {
        let removed = {
            let mut queue = self.disk.queue();
            let mut removed = Vec::new();
            let mut keep = Vec::new();
            for slot in queue.slots.drain(..) {
                if pred(&slot) {
                    removed.push(slot);
                } else {
                    keep.push(slot);
                }
            }
            queue.slots = keep;
            removed
        };
        for slot in removed {
            self.release_queued_offset(slot.offset, slot.generation, slot.byte_len);
            self.mark_group_slot_done(slot.group, Some(clone_error(error)));
        }
    }

    fn release_queued_offset(&self, offset: PartitionChunkOffset, generation: u64, byte_len: u64) {
        let mut pending = self.pending_writes.write().unwrap();
        pending.queued_unpacked_bytes = pending.queued_unpacked_bytes.saturating_sub(byte_len);
        if pending.occupancy.get(&offset).copied() == Some(generation) {
            pending.occupancy.remove(&offset);
        }
    }

    fn mark_group_slot_done(&self, group: u64, error: Option<WriteDataChunkError>) {
        {
            let mut queue = self.disk.queue();
            let finish = {
                let Some(entry) = queue.groups.get_mut(&group) else {
                    return;
                };
                if entry.failed.is_none() {
                    entry.failed = error;
                }
                entry.remaining = entry.remaining.saturating_sub(1);
                if entry.remaining != 0 {
                    return;
                }
                let notify = entry.notify.take();
                let waited = entry.waited;
                if notify.is_some() || !waited {
                    Some((notify, entry.failed.take()))
                } else {
                    None
                }
            };
            if let Some((notify, failed)) = finish {
                queue.groups.remove(&group);
                if let Some(tx) = notify {
                    let result = match failed {
                        Some(error) => Err(error),
                        None => Ok(()),
                    };
                    // Send before the queue lock drops so a timed-out waiter
                    // that then calls try_recv cannot miss this result.
                    let _ = tx.send(result);
                }
            }
        }
        self.disk.notify();
    }

    pub(super) fn poll_acks(&self) {
        let inflight = {
            let mut queue = self.disk.queue();
            std::mem::take(&mut queue.inflight)
        };
        let mut again = Vec::new();
        for item in inflight {
            match item.done.try_recv() {
                Ok(result) => self.finish_inflight(item, result),
                Err(TryRecvError::Empty) => again.push(item),
                Err(TryRecvError::Disconnected) => self.finish_inflight(
                    item,
                    Err(WriteDataChunkError::Other(eyre::eyre!(
                        "index drain closed"
                    ))),
                ),
            }
        }
        if !again.is_empty() {
            self.disk.queue().inflight.extend(again);
        }
    }

    fn finish_inflight(&self, item: Inflight, result: Result<(), WriteDataChunkError>) {
        let already_failed = self
            .disk
            .queue()
            .groups
            .get(&item.group)
            .is_some_and(|entry| entry.failed.is_some());
        let Inflight {
            group,
            offset,
            packed,
            generation,
            priority,
            byte_len,
            release_id,
            ..
        } = item;
        if let Some(id) = release_id {
            self.finish_released(
                id,
                already_failed,
                ReleasedChunk {
                    group,
                    offset,
                    packed,
                    generation,
                    priority,
                    byte_len,
                },
                result,
            );
            return;
        }
        let error = match result {
            Ok(()) if already_failed => {
                self.release_queued_offset(offset, generation, byte_len);
                None
            }
            Ok(()) => {
                let inserted = self.commit_packed(offset, packed, generation, priority, byte_len);
                if inserted {
                    None
                } else {
                    Some(WriteDataChunkError::WritesPaused)
                }
            }
            Err(error) => {
                self.release_queued_offset(offset, generation, byte_len);
                Some(error)
            }
        };
        self.mark_group_slot_done(group, error);
    }

    /// Count one island member. Insert only when every member has been counted,
    /// and insert each surviving contiguous piece under one lock.
    fn finish_released(
        &self,
        id: u64,
        already_failed: bool,
        chunk: ReleasedChunk,
        result: Result<(), WriteDataChunkError>,
    ) {
        let ready = match result {
            Ok(()) if already_failed => {
                self.release_queued_offset(chunk.offset, chunk.generation, chunk.byte_len);
                self.mark_group_slot_done(chunk.group, None);
                None
            }
            Ok(()) => Some(chunk),
            Err(error) => {
                self.release_queued_offset(chunk.offset, chunk.generation, chunk.byte_len);
                self.mark_group_slot_done(chunk.group, Some(error));
                None
            }
        };
        match self.account_release(id, ready) {
            AccountedRelease::Open => {}
            AccountedRelease::Finished(release) => self.commit_release(release),
            AccountedRelease::Missing(Some(chunk)) => {
                let inserted = self.commit_packed(
                    chunk.offset,
                    chunk.packed,
                    chunk.generation,
                    chunk.priority,
                    chunk.byte_len,
                );
                let error = if inserted {
                    None
                } else {
                    Some(WriteDataChunkError::WritesPaused)
                };
                self.mark_group_slot_done(chunk.group, error);
            }
            AccountedRelease::Missing(None) => {}
        }
    }

    fn note_release_gap(&self, release_id: Option<u64>) {
        let Some(id) = release_id else {
            return;
        };
        if let AccountedRelease::Finished(release) = self.account_release(id, None) {
            self.commit_release(release);
        }
    }

    fn account_release(&self, id: u64, ready: Option<ReleasedChunk>) -> AccountedRelease {
        let mut queue = self.disk.queue();
        let finished = {
            let Some(release) = queue.releases.get_mut(&id) else {
                return AccountedRelease::Missing(ready);
            };
            if let Some(chunk) = ready {
                release.ready.push(chunk);
            }
            release.left = release.left.saturating_sub(1);
            release.left == 0
        };
        if !finished {
            return AccountedRelease::Open;
        }
        match queue.releases.remove(&id) {
            Some(release) => AccountedRelease::Finished(release),
            None => AccountedRelease::Missing(None),
        }
    }

    fn commit_release(&self, release: SpanRelease) {
        let mut kept = Vec::with_capacity(release.ready.len());
        for chunk in release.ready {
            let group_failed = self
                .disk
                .queue()
                .groups
                .get(&chunk.group)
                .is_some_and(|entry| entry.failed.is_some());
            if group_failed {
                self.release_queued_offset(chunk.offset, chunk.generation, chunk.byte_len);
                self.mark_group_slot_done(chunk.group, None);
                continue;
            }
            kept.push(chunk);
        }
        kept.sort_by_key(|chunk| chunk.offset);
        let mut piece: Vec<ReleasedChunk> = Vec::new();
        for chunk in kept {
            let joins = piece
                .last()
                .is_some_and(|prev| prev.offset.0.checked_add(1) == Some(chunk.offset.0));
            if !piece.is_empty() && !joins {
                self.insert_packed_piece(&mut piece);
            }
            piece.push(chunk);
        }
        self.insert_packed_piece(&mut piece);
    }

    /// Insert one contiguous piece. Every member shares the best priority in
    /// the piece and one queue time, so the joiner does not split the piece.
    fn insert_packed_piece(&self, piece: &mut Vec<ReleasedChunk>) {
        if piece.is_empty() {
            return;
        }
        let priority = piece
            .iter()
            .map(|chunk| chunk.priority)
            .min()
            .expect("packed piece");
        let queued_at = Instant::now();
        let mut outcomes = Vec::with_capacity(piece.len());
        let mut wrote = false;
        {
            let mut pending = self.pending_writes.write().unwrap();
            for chunk in piece.drain(..) {
                pending.queued_unpacked_bytes =
                    pending.queued_unpacked_bytes.saturating_sub(chunk.byte_len);
                if pending.occupancy.get(&chunk.offset).copied() != Some(chunk.generation) {
                    outcomes.push((chunk.group, false));
                    continue;
                }
                pending.occupancy.remove(&chunk.offset);
                if self.data_writes_paused()
                    || self.index_write_generation.load(Ordering::SeqCst) != chunk.generation
                {
                    outcomes.push((chunk.group, false));
                    continue;
                }
                pending.insert(chunk.offset, (chunk.packed, ChunkType::Data));
                pending.priorities.insert(chunk.offset, priority);
                pending.queued_at.insert(chunk.offset, queued_at);
                wrote = true;
                outcomes.push((chunk.group, true));
            }
            if wrote {
                *self.last_pending_write.write().unwrap() = queued_at;
            }
        }
        if wrote {
            self.disk.notify();
        }
        for (group, inserted) in outcomes {
            let error = if inserted {
                None
            } else {
                Some(WriteDataChunkError::WritesPaused)
            };
            self.mark_group_slot_done(group, error);
        }
    }

    fn commit_packed(
        &self,
        offset: PartitionChunkOffset,
        packed: Vec<u8>,
        generation: u64,
        priority: WritePriority,
        unpacked_len: u64,
    ) -> bool {
        let mut pending = self.pending_writes.write().unwrap();
        pending.queued_unpacked_bytes = pending.queued_unpacked_bytes.saturating_sub(unpacked_len);
        if pending.occupancy.get(&offset).copied() != Some(generation) {
            return false;
        }
        pending.occupancy.remove(&offset);
        if self.data_writes_paused()
            || self.index_write_generation.load(Ordering::SeqCst) != generation
        {
            return false;
        }
        pending.insert(offset, (packed, ChunkType::Data));
        pending.priorities.insert(offset, priority);
        pending.queued_at.insert(offset, Instant::now());
        *self.last_pending_write.write().unwrap() = Instant::now();
        drop(pending);
        self.disk.notify();
        true
    }

    fn hold_cap_bytes(&self) -> u64 {
        let chunk = self.config.consensus.chunk_size.max(1);
        self.config
            .node_config
            .storage
            .entropy_sweep_max_bytes
            .max(chunk)
    }

    #[cfg(test)]
    pub(super) fn drive_entropy_queue_for_test(&self) {
        self.drain_entropy_queue();
    }

    #[cfg(test)]
    pub(super) fn sweep_one_for_test(&self) -> bool {
        self.sweep_one()
    }

    #[cfg(test)]
    pub(super) fn sweep_slot_count_for_test(&self) -> usize {
        self.disk.queue().slots.len()
    }

    #[cfg(test)]
    pub(super) fn inflight_len_for_test(&self) -> usize {
        self.disk.queue().inflight.len()
    }

    /// Finish one index ack and leave any other ready ack in the queue.
    #[cfg(test)]
    pub(super) fn finish_one_ready_ack_for_test(&self) -> bool {
        let inflight = {
            let mut queue = self.disk.queue();
            std::mem::take(&mut queue.inflight)
        };
        let mut again = Vec::new();
        let mut finished = false;
        for item in inflight {
            if finished {
                again.push(item);
                continue;
            }
            match item.done.try_recv() {
                Ok(result) => {
                    self.finish_inflight(item, result);
                    finished = true;
                }
                Err(TryRecvError::Empty) => again.push(item),
                Err(TryRecvError::Disconnected) => {
                    self.finish_inflight(
                        item,
                        Err(WriteDataChunkError::Other(eyre::eyre!(
                            "index drain closed"
                        ))),
                    );
                    finished = true;
                }
            }
        }
        if !again.is_empty() {
            self.disk.queue().inflight.extend(again);
        }
        finished
    }

    /// Lane path. Up to `INFLIGHT_WRITES` pwrites share the module file.
    /// A recall stops new submits. Calls already submitted keep running.
    pub(super) fn write_runs_window(&self, runs: &[WriteRun]) -> eyre::Result<()> {
        self.disk.wait_until_idle();
        let mut next = 0;
        while next < runs.len() {
            let mut guards = Vec::with_capacity(INFLIGHT_WRITES);
            while guards.len() < INFLIGHT_WRITES && next + guards.len() < runs.len() {
                let bytes = runs[next + guards.len()].bytes.len() as u64;
                if self.disk.recall_pending() || !self.disk.try_submit_write(INFLIGHT_WRITES, bytes)
                {
                    break;
                }
                guards.push(OpGuard {
                    disk: &self.disk,
                    bytes,
                });
            }
            if guards.is_empty() {
                if self.disk.recall_pending() {
                    self.disk.yield_to_recall();
                } else {
                    let bytes = runs[next].bytes.len() as u64;
                    self.disk.wait_for_write_slot(bytes);
                }
                continue;
            }
            let batch = &runs[next..next + guards.len()];
            let reserved = guards.len();
            let results = std::thread::scope(|scope| {
                let mut handles = Vec::with_capacity(reserved);
                for (run, guard) in batch.iter().zip(guards) {
                    handles.push(scope.spawn(move || self.pwrite_run_inflight(run, guard)));
                }
                handles
                    .into_iter()
                    .map(|handle| handle.join().expect("chunk write"))
                    .collect::<Vec<_>>()
            });
            next += reserved;
            for result in results {
                result?;
            }
        }
        Ok(())
    }

    /// Kernel write on a dup'd fd. The mutex is held only to dup, and this
    /// call is already counted in `disk_ops`, so it must not take the
    /// exclusive lock or wait for a recall. The count drops before the
    /// interval update, which does not touch the chunk file.
    fn pwrite_run_inflight(&self, run: &WriteRun, guard: OpGuard<'_>) -> eyre::Result<()> {
        let chunk_size = self.config.consensus.chunk_size;
        let (file_arc, file_offset, submodule_offset) = {
            let (interval, submodule) = self.get_submodule_for_offset(run.start)?;
            let submodule_offset = run.start - interval.start();
            (
                Arc::clone(&submodule.file),
                u64::from(submodule_offset) * chunk_size,
                submodule_offset,
            )
        };
        let start_time = Instant::now();
        let write_result = {
            let _guard = guard;
            let cloned = {
                let file = file_arc.lock().unwrap_or_else(PoisonError::into_inner);
                file.try_clone()
            };
            match cloned {
                Ok(file) => file
                    .write_all_at(run.bytes.as_slice(), file_offset)
                    .map_err(|error| Self::chunk_write_report(run.start, submodule_offset, &error)),
                Err(error) => Err(Self::chunk_write_report(
                    run.start,
                    submodule_offset,
                    &error,
                )),
            }
        };
        self.note_write_run(run, start_time, write_result)
    }

    fn chunk_write_report(
        start: PartitionChunkOffset,
        submodule_offset: impl std::fmt::Display,
        error: &dyn std::fmt::Display,
    ) -> eyre::Report {
        tracing::error!(
            "Failed to write chunk @ chunk_offset {} submodule_offset {}: {}",
            start,
            submodule_offset,
            error
        );
        eyre::eyre!(
            "Failed to write chunk @ chunk_offset {} submodule_offset {}: {}",
            start,
            submodule_offset,
            error
        )
    }

    /// Start the thread that sweeps and flushes this module. `StorageModule::new`
    /// does not call this: unit tests drive the sweep on the caller.
    pub fn spawn_disk_lane(
        self: &Arc<Self>,
    ) -> std::io::Result<Option<std::thread::JoinHandle<()>>> {
        if !self.disk.prepare_lane() {
            return Ok(None);
        }
        let module = Arc::clone(self);
        match std::thread::Builder::new()
            .name(format!("disk-lane-{}", self.id))
            .spawn(move || module.run_disk_lane())
        {
            Ok(handle) => Ok(Some(handle)),
            Err(error) => {
                self.disk.clear_lane();
                Err(error)
            }
        }
    }

    pub fn stop_disk_lane(&self) {
        self.disk.request_stop();
    }

    pub fn wake_disk_lane(&self) {
        self.disk.notify();
    }

    pub fn disk_lane_running(&self) -> bool {
        self.disk.lane_running()
    }

    fn run_disk_lane(&self) {
        self.disk.bind_lane_thread();
        let _exit = LaneExit { gate: &self.disk };
        let mut seen = 0;
        loop {
            if self.disk.stop_requested() {
                break;
            }
            self.disk_lane_pass();
            if self.disk.stop_requested() {
                break;
            }
            seen = self.wait_until_lane_event(seen);
        }
        self.finish_disk_lane();
    }

    /// Sleep until the lane has work. `seen` is the wake epoch at the previous
    /// poll. An index ack bumps that epoch under the same lock, so the ack is
    /// polled before the next sleep.
    fn wait_until_lane_event(&self, mut seen: u64) -> u64 {
        loop {
            if self.disk.stop_requested() || self.lane_work_ready() {
                return self.disk.wake.epoch();
            }
            // A zero delay means a command is already due. If the disk is
            // still busy, `lane_work_ready` is false and the next notify
            // (a finished command) is the wake. A positive delay is the
            // rest of the write grace or the read grace, whichever is sooner.
            let timeout = self.lane_grace_remaining().filter(|delay| !delay.is_zero());
            let mut guard = self.disk.lock_wake();
            if self.disk.stop_requested() {
                return self.disk.wake.epoch();
            }
            let epoch = self.disk.wake.epoch();
            if epoch != seen {
                drop(guard);
                self.poll_acks();
                seen = epoch;
                continue;
            }
            guard = match timeout {
                Some(delay) => self.disk.wake.wait_timeout(guard, delay),
                None => self.disk.wake.wait(guard),
            };
            drop(guard);
        }
    }

    /// Time until the next packed write is allowed.
    /// `None` when nothing is queued. Zero when the disk is idle, a full run
    /// is ready, the durability count is reached, or an aged short run outside
    /// the short-run hold is ready. A hold after one short run keeps the other
    /// short runs queued while the disk stays busy.
    fn write_grace_remaining(&self) -> Option<Duration> {
        let pending = self.pending_writes.read().unwrap();
        let oldest = pending.queued_at.values().copied().min()?;
        if self.disk.chunk_disk_idle() {
            return Some(Duration::ZERO);
        }
        let chunk = self.config.consensus.chunk_size.max(1);
        let grace = reorder_grace(chunk);
        let until_aged = grace.saturating_sub(oldest.elapsed());
        if self.short_runs_due(pending.len())
            || self.has_full_write_run(&pending, WRITE_RUN_MAX_BYTES.max(chunk))
        {
            return Some(Duration::ZERO);
        }
        let until_hold = self.disk.short_hold_remaining().unwrap_or(Duration::ZERO);
        Some(until_aged.max(until_hold))
    }

    /// Time until the next entropy read is allowed.
    /// `None` when no disk slot is queued. Zero when the disk is idle, a full
    /// span is ready, the durability count is reached, a recall flush is owed,
    /// or an aged short span outside the read hold is ready. A hold after one
    /// short span keeps the others queued while the disk stays busy.
    fn read_grace_remaining(&self) -> Option<Duration> {
        let queue = self.disk.queue();
        let spans = self.maximal_disk_spans(&queue.slots);
        if spans.is_empty() {
            return None;
        }
        if self.disk.chunk_disk_idle() {
            return Some(Duration::ZERO);
        }
        let disk_slots = spans.iter().map(|span| span.indexes.len()).sum::<usize>();
        let cap = self.hold_cap_bytes();
        let chunk = self.config.consensus.chunk_size.max(1);
        let grace = reorder_grace(chunk);
        if self.disk_reads_due(disk_slots)
            || spans.iter().any(|span| span.bytes >= cap)
            || (spans.iter().any(|span| span.aged) && !self.disk.short_reads_held())
        {
            return Some(Duration::ZERO);
        }
        let oldest = spans.iter().map(|span| span.oldest).min()?;
        let until_aged = grace.saturating_sub(oldest.elapsed());
        let until_hold = self
            .disk
            .read_short_hold_remaining()
            .unwrap_or(Duration::ZERO);
        Some(until_aged.max(until_hold))
    }

    /// Soonest of the write grace and the read grace. `None` only when both
    /// queues have nothing waiting on a timer.
    fn lane_grace_remaining(&self) -> Option<Duration> {
        match (self.write_grace_remaining(), self.read_grace_remaining()) {
            (Some(write), Some(read)) => Some(write.min(read)),
            (Some(delay), None) | (None, Some(delay)) => Some(delay),
            (None, None) => None,
        }
    }

    /// True when the longest ready entropy span is strictly longer than the
    /// longest packed run that is allowed to hit the disk now.
    fn entropy_outranks_writes(&self) -> bool {
        let read_bytes = self.longest_disk_span_bytes();
        read_bytes > self.longest_ready_write_bytes()
    }

    /// Ready to run a pass. A recall, a file holder, or a full window is not
    /// ready: the loop waits for the notify. An idle disk with a queued short
    /// command is ready, so the lane does not sleep on `reorder_grace`. While
    /// the disk is busy, a short packed run waits for the cap, the durability
    /// count, or `reorder_grace`, and after one short run the others wait
    /// another grace. A short entropy span follows the same rule on the read
    /// hold. A full run, an owed recall flush, the one eligible short run, a
    /// ready entropy span, memory entropy, or an external flush or interval
    /// write is ready.
    fn lane_work_ready(&self) -> bool {
        if !self.disk.disk_available() {
            return false;
        }
        if self.disk.lane_request_pending()
            || self.disk.recall_flush_is_owed()
            || self.pending_run_ready()
            || self.has_ready_disk_span()
        {
            return true;
        }
        // Memory entropy needs the sweep lock. A young short disk span stays
        // queued, and it does not wake the lane on its own.
        self.disk.sweep_lock_free() && self.memory_entropy_can_fold()
    }

    /// Buffered entropy at the best queued priority. A disk slot of that
    /// priority keeps the file read ahead of the fold.
    fn memory_entropy_can_fold(&self) -> bool {
        let queue = self.disk.queue();
        let Some(best) = queue.slots.iter().map(|slot| slot.priority).min() else {
            return false;
        };
        queue
            .slots
            .iter()
            .any(|slot| slot.priority == best && slot.pending_entropy.is_some())
    }

    fn disk_lane_pass(&self) {
        // Recall sets `recall_waiters` before it reads, and it already pinned
        // the index gap. Submit nothing until that hold drops.
        if self.disk.recall_pending() {
            self.poll_acks();
            return;
        }
        // Queued index commits finish here, before this pass reads or writes
        // `chunks.dat`. Commits that this pass queues wait until the guard drops.
        let chunk_io = self.disk.index_gap.hold_for_chunk_io();
        self.poll_acks();
        if self.disk.recall_pending() {
            drop(chunk_io);
            return;
        }
        // An external flush or interval rewrite runs here, before this pass
        // chooses a chunk command. A recall above skips it until the `pread`
        // returns, so the mining read is not cut by the file update.
        self.service_lane_handoff();
        // Capture this before the flush clears it. Entropy stays behind an
        // owed recall flush so the waiting recall gets the disk next.
        // Capture the read-vs-write choice once. A sweep removes the span
        // that won, and this pass must not start a second sweep after it
        // already chose reads.
        let owed_flush = self.disk.recall_flush_is_owed();
        let read_first = !owed_flush && self.entropy_outranks_writes();
        if owed_flush {
            if let Err(error) = self.flush_packed_window() {
                tracing::error!(
                    "Couldn't flush packed chunks after recall for storage_module {}: {error}",
                    self.id
                );
            }
        } else if read_first {
            self.pump_entropy_reads();
            if !self.disk.recall_pending() && self.pending_run_ready() {
                self.write_ready_runs();
            }
        } else if self.pending_run_ready() {
            self.write_ready_runs();
        }
        if !owed_flush && !read_first && !self.disk.recall_pending() {
            self.pump_entropy_reads();
        }
    }

    fn write_ready_runs(&self) {
        if let Err(error) = self.commit_pending_runs(false, Some(INFLIGHT_WRITES), true) {
            tracing::error!(
                "Couldn't write packed chunks for storage_module {}: {error}",
                self.id
            );
        }
    }

    /// Run posted flushes and interval rewrites. Each request's chunk write
    /// still persists intervals before the `pwrite`s and after the `fsync`.
    fn service_lane_handoff(&self) {
        loop {
            let Some(request) = self.disk.take_lane_request() else {
                return;
            };
            let mut error = None;
            if request.force
                && let Err(cause) = self.commit_pending_runs(true, None, false)
            {
                tracing::error!(
                    "Couldn't flush packed chunks for storage_module {}: {cause}",
                    self.id
                );
                error = Some(cause.to_string());
            }
            if request.persist
                && let Err(cause) = self.write_intervals_files(None)
            {
                tracing::error!(
                    "Couldn't write intervals for storage_module {}: {cause}",
                    self.id
                );
                if error.is_none() {
                    error = Some(cause.to_string());
                }
            }
            self.disk.finish_lane_request(request.mark, error);
        }
    }

    fn finish_disk_lane(&self) {
        // Shutdown still persists every pending chunk. Clear the flag first so
        // that flush is not skipped as an owed window. Posted waits are
        // drained on this thread before the lane drops, and a waiter that
        // arrives after the drop writes on its own thread.
        self.disk.clear_recall_flush();
        self.service_lane_handoff();
        if let Err(error) = self.force_sync_pending_chunks() {
            tracing::error!(
                "Couldn't flush storage module {} on disk lane stop: {error}",
                self.id
            );
        }
        self.service_lane_handoff();
        self.poll_acks();
    }
}

struct LaneExit<'a> {
    gate: &'a DiskGate,
}

impl Drop for LaneExit<'_> {
    fn drop(&mut self) {
        self.gate.clear_recall_flush();
        self.gate.clear_lane();
        self.gate.unbind_lane_thread();
        self.gate.wake_lane_handoff();
    }
}

fn entropy_read_unfinished() -> WriteDataChunkError {
    WriteDataChunkError::Other(eyre::eyre!("entropy read did not finish"))
}

struct SpanAnchor {
    index: usize,
    offset: u32,
    queued_at: Instant,
    priority: WritePriority,
}

struct DiskSpan {
    indexes: Vec<usize>,
    bytes: u64,
    oldest: Instant,
    aged: bool,
    priority: WritePriority,
    start: u32,
}

fn span_command_bytes(start: u32, end: u32, chunk_size: u64) -> u64 {
    u64::from(end.saturating_sub(start).saturating_add(1)).saturating_mul(chunk_size)
}

fn span_can_grow(
    start: &SpanAnchor,
    end: &SpanAnchor,
    next: &SpanAnchor,
    chunk_size: u64,
    hole_bytes: u64,
    max_bytes: u64,
) -> bool {
    if next.offset <= end.offset {
        return false;
    }
    let gap_bytes = u64::from(next.offset - end.offset - 1).saturating_mul(chunk_size);
    let bytes = span_command_bytes(start.offset, next.offset, chunk_size);
    gap_bytes <= hole_bytes && bytes <= max_bytes
}

fn partition_file_spans(
    anchors: &[SpanAnchor],
    chunk_size: u64,
    hole_bytes: u64,
    max_bytes: u64,
    grace: Duration,
) -> Vec<DiskSpan> {
    let mut spans = Vec::new();
    let mut start = 0usize;
    while start < anchors.len() {
        let mut end = start;
        while end + 1 < anchors.len()
            && span_can_grow(
                &anchors[start],
                &anchors[end],
                &anchors[end + 1],
                chunk_size,
                hole_bytes,
                max_bytes,
            )
        {
            end += 1;
        }
        spans.push(disk_span_from(&anchors[start..=end], chunk_size, grace));
        start = end + 1;
    }
    spans
}

fn disk_span_from(anchors: &[SpanAnchor], chunk_size: u64, grace: Duration) -> DiskSpan {
    let start = anchors[0].offset;
    let end = anchors[anchors.len() - 1].offset;
    let oldest = anchors
        .iter()
        .map(|anchor| anchor.queued_at)
        .min()
        .unwrap_or(anchors[0].queued_at);
    let priority = anchors
        .iter()
        .map(|anchor| anchor.priority)
        .min()
        .unwrap_or(anchors[0].priority);
    DiskSpan {
        indexes: anchors.iter().map(|anchor| anchor.index).collect(),
        bytes: span_command_bytes(start, end, chunk_size),
        oldest,
        aged: oldest.elapsed() >= grace,
        priority,
        start,
    }
}

fn disk_slot_count(slots: &[SweepSlot]) -> usize {
    slots
        .iter()
        .filter(|slot| slot.pending_entropy.is_none())
        .count()
}

fn pread_span_bytes(span: &[SweepSlot], chunk_size: u64) -> u64 {
    let (Some(first), Some(last)) = (span.first(), span.last()) else {
        return 0;
    };
    (u64::from(last.offset.0.saturating_sub(first.offset.0)) + 1).saturating_mul(chunk_size)
}

fn clone_error(error: &WriteDataChunkError) -> WriteDataChunkError {
    match error {
        WriteDataChunkError::DataRootNotFound => WriteDataChunkError::DataRootNotFound,
        WriteDataChunkError::WritesPaused => WriteDataChunkError::WritesPaused,
        WriteDataChunkError::Other(report) => WriteDataChunkError::Other(eyre::eyre!("{report}")),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, atomic::Ordering, mpsc},
        thread,
        time::{Duration, Instant},
    };

    use super::{DiskGate, INFLIGHT_DISK_OPS, INFLIGHT_WRITES};
    use irys_types::{
        ConsensusConfig, ConsensusOptions, H256, NodeConfig, PartitionChunkOffset,
        StorageSyncConfig,
    };
    use nodit::interval::ii;

    #[test]
    fn write_window_stops_at_the_inflight_limit() {
        let disk = DiskGate::new();
        for _ in 0..INFLIGHT_WRITES {
            assert!(disk.try_submit_write(INFLIGHT_WRITES, 0));
        }
        assert!(!disk.try_submit_write(INFLIGHT_WRITES, 0));
        assert_eq!(disk.disk_ops.load(Ordering::SeqCst), INFLIGHT_WRITES);
        for _ in 0..INFLIGHT_WRITES {
            disk.finish_op(0);
        }
        assert_eq!(disk.disk_ops.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn recall_starts_while_a_write_is_inflight() {
        let disk = Arc::new(DiskGate::new());
        assert!(disk.try_submit_write(INFLIGHT_WRITES, 0));
        let waiting = Arc::clone(&disk);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let recall = thread::spawn(move || {
            let hold = waiting.begin_recall();
            entered_tx.send(()).expect("recall entered");
            release_rx.recv().expect("release");
            drop(hold);
        });

        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("recall waited for the inflight write");
        assert!(disk.recall_pending());
        assert!(!disk.try_submit_write(INFLIGHT_WRITES, 0));
        assert_eq!(disk.disk_ops.load(Ordering::SeqCst), 1);
        release_tx.send(()).expect("release send");
        recall.join().expect("recall thread");
        assert!(!disk.recall_pending());
        disk.finish_op(0);
        assert_eq!(disk.disk_ops.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn write_hold_blocks_entropy_reads() {
        let disk = DiskGate::new();
        let _hold = disk.hold_writes();
        assert!(!disk.try_submit_read(INFLIGHT_DISK_OPS, 0));
        assert!(disk.try_submit_write(INFLIGHT_DISK_OPS, 0));
        assert!(disk.try_submit_write(INFLIGHT_DISK_OPS, 0));
        assert!(disk.try_submit_write(INFLIGHT_DISK_OPS, 0));
        assert!(!disk.try_submit_write(INFLIGHT_DISK_OPS, 0));
        disk.finish_op(0);
        disk.finish_op(0);
        disk.finish_op(0);
    }

    #[test]
    fn exclusive_io_waits_for_inflight_and_rejects_submit() {
        let disk = Arc::new(DiskGate::new());
        assert!(disk.try_submit_read(INFLIGHT_DISK_OPS, 0));
        let waiting = Arc::clone(&disk);
        let (done_tx, done_rx) = mpsc::channel();
        let exclusive = thread::spawn(move || {
            waiting.begin_exclusive_io();
            waiting.end_exclusive_io();
            done_tx.send(()).expect("exclusive done");
        });

        let started = Instant::now();
        while disk.io_exclusive.load(Ordering::SeqCst) == 0 {
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "exclusive io did not start"
            );
            thread::yield_now();
        }
        assert!(!disk.try_submit_write(INFLIGHT_DISK_OPS, 0));
        assert!(done_rx.try_recv().is_err());
        disk.finish_op(0);
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("exclusive io did not drain inflight ops");
        exclusive.join().expect("exclusive thread");
        assert_eq!(disk.io_exclusive.load(Ordering::SeqCst), 0);
        assert!(disk.try_submit_read(INFLIGHT_DISK_OPS, 0));
        disk.finish_op(0);
    }

    #[test]
    fn begin_recall_waits_for_owed_flush_without_holding_the_disk() {
        let disk = Arc::new(DiskGate::new());
        disk.arm_recall_flush();
        let waiting = Arc::clone(&disk);
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let recall = thread::spawn(move || {
            started_tx.send(()).expect("recall thread");
            let _hold = waiting.begin_recall();
            done_tx.send(()).expect("recall entered");
        });
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("recall thread did not start");
        let started = Instant::now();
        while started.elapsed() < Duration::from_millis(50) {
            assert_eq!(
                disk.recall_waiters.load(Ordering::SeqCst),
                0,
                "recall held the disk while a flush was owed"
            );
            assert!(
                done_rx.try_recv().is_err(),
                "recall started during the owed flush"
            );
            thread::yield_now();
        }
        disk.clear_recall_flush();
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("recall did not start after the flush cleared");
        recall.join().expect("recall thread");
        assert!(!disk.recall_pending());
    }

    #[test]
    fn write_budget_stops_before_five_hundred_ms() {
        let disk = DiskGate::new();
        let ten_mib = 10 * 1024 * 1024;
        for _ in 0..8 {
            assert!(disk.try_submit_write(INFLIGHT_WRITES, ten_mib));
        }
        assert!(!disk.try_submit_write(INFLIGHT_WRITES, ten_mib));
        assert_eq!(disk.disk_ops.load(Ordering::SeqCst), 8);
        for _ in 0..8 {
            disk.finish_op(ten_mib);
        }
        assert_eq!(disk.inflight_bytes.load(Ordering::SeqCst), 0);
        let huge = 200 * 1024 * 1024;
        assert!(disk.try_submit_write(INFLIGHT_WRITES, huge));
        assert!(!disk.try_submit_write(INFLIGHT_WRITES, 1));
        disk.finish_op(huge);
        assert_eq!(disk.disk_ops.load(Ordering::SeqCst), 0);
        assert_eq!(disk.inflight_bytes.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn take_disk_span_orders_slots_by_offset() -> eyre::Result<()> {
        let (_tmp, module) = span_order_fixture()?;
        let queue_order = [2_u32, 0, 1];
        {
            let mut queue = module.disk.queue();
            for offset in queue_order {
                queue.slots.push(sweep_slot(offset));
            }
            let queued_at = Instant::now()
                .checked_sub(Duration::from_secs(2))
                .expect("test clock");
            for slot in &mut queue.slots {
                slot.queued_at = queued_at;
            }
        }
        let span = module.take_disk_span().expect("span");
        let offsets: Vec<u32> = span.iter().map(|slot| slot.offset.0).collect();
        assert_eq!(offsets, vec![0, 1, 2]);
        Ok(())
    }

    fn sweep_slot(offset: u32) -> super::SweepSlot {
        super::SweepSlot {
            group: 1,
            offset: PartitionChunkOffset::from(offset),
            unpacked: Arc::new(vec![0; 32]),
            data_path: Arc::new(Vec::new()),
            path_hash: H256::zero(),
            generation: 0,
            priority: super::WritePriority::Ingress,
            pending_entropy: None,
            byte_len: 32,
            queued_at: Instant::now(),
        }
    }

    #[test]
    fn reorder_grace_covers_one_scan_of_full_runs() {
        let small = super::reorder_grace(32);
        let production = super::reorder_grace(256 * 1024);
        assert!(small > Duration::from_millis(50));
        assert!(small < Duration::from_millis(200));
        assert!(production > Duration::from_secs(10));
        assert!(production < Duration::from_secs(20));
    }

    #[test]
    fn due_reads_pick_the_longer_span() -> eyre::Result<()> {
        let (_tmp, module) = span_fixture("span_rank", 8, 8, 1)?;
        {
            let mut queue = module.disk.queue();
            for offset in [0_u32, 3, 4, 5] {
                queue.slots.push(sweep_slot(offset));
            }
        }
        module.disk.arm_recall_flush();
        let span = module.take_disk_span().expect("span");
        module.disk.clear_recall_flush();
        let offsets: Vec<u32> = span.iter().map(|slot| slot.offset.0).collect();
        assert_eq!(offsets, vec![3, 4, 5]);
        Ok(())
    }

    #[test]
    fn young_short_span_stays_queued_while_the_disk_is_busy() -> eyre::Result<()> {
        let (_tmp, module) = span_fixture("span_young", 8, 8, 1)?;
        {
            let mut queue = module.disk.queue();
            for offset in [0_u32, 3, 4, 5] {
                queue.slots.push(sweep_slot(offset));
            }
        }
        module.disk.occupy_for_test();
        assert!(module.take_disk_span().is_none());
        assert_eq!(module.sweep_slot_count_for_test(), 4);
        module.disk.release_for_test();
        let span = module
            .take_disk_span()
            .expect("idle disk reads the longer span");
        let offsets: Vec<u32> = span.iter().map(|slot| slot.offset.0).collect();
        assert_eq!(offsets, vec![3, 4, 5]);
        Ok(())
    }

    #[test]
    fn full_span_is_read_while_young() -> eyre::Result<()> {
        let (_tmp, module) = span_fixture("span_full", 8, 8, 1)?;
        {
            let mut queue = module.disk.queue();
            for offset in 0..8 {
                queue.slots.push(sweep_slot(offset));
            }
        }
        let span = module.take_disk_span().expect("span");
        let offsets: Vec<u32> = span.iter().map(|slot| slot.offset.0).collect();
        assert_eq!(offsets, (0..8).collect::<Vec<_>>());
        Ok(())
    }

    #[test]
    fn oldest_short_span_is_read_then_the_hold_blocks_the_next() -> eyre::Result<()> {
        let (_tmp, module) = span_fixture("span_hold", 8, 8, 1)?;
        {
            let mut queue = module.disk.queue();
            for offset in [0_u32, 3, 4, 5] {
                queue.slots.push(sweep_slot(offset));
            }
            let now = Instant::now();
            queue.slots[0].queued_at = now.checked_sub(Duration::from_secs(5)).expect("test clock");
            for slot in queue.slots.iter_mut().skip(1) {
                slot.queued_at = now.checked_sub(Duration::from_secs(2)).expect("test clock");
            }
        }
        module.disk.occupy_for_test();
        let span = module.take_disk_span().expect("span");
        let offsets: Vec<u32> = span.iter().map(|slot| slot.offset.0).collect();
        assert_eq!(offsets, vec![0]);
        module.disk.arm_read_short_hold(super::reorder_grace(32));
        assert!(module.take_disk_span().is_none());
        assert_eq!(module.sweep_slot_count_for_test(), 3);
        module.disk.release_for_test();
        let released = module
            .take_disk_span()
            .expect("idle disk reads the held span");
        let released_offsets: Vec<u32> = released.iter().map(|slot| slot.offset.0).collect();
        assert_eq!(released_offsets, vec![3, 4, 5]);
        Ok(())
    }

    #[test]
    fn aged_short_span_is_read_ahead_of_a_full_span() -> eyre::Result<()> {
        let (_tmp, module) = span_fixture("span_short_first", 16, 8, 1)?;
        {
            let mut queue = module.disk.queue();
            for offset in 0..8 {
                queue.slots.push(sweep_slot(offset));
            }
            queue.slots.push(sweep_slot(10));
            queue.slots[8].queued_at = Instant::now()
                .checked_sub(Duration::from_secs(2))
                .expect("test clock");
        }
        // An idle disk ranks the full span first. The short span leads only
        // while another command keeps the disk busy.
        module.disk.occupy_for_test();
        let first = module.take_disk_span().expect("short");
        assert_eq!(
            first.iter().map(|slot| slot.offset.0).collect::<Vec<_>>(),
            vec![10]
        );
        module.disk.arm_read_short_hold(super::reorder_grace(32));
        let full = module.take_disk_span().expect("full");
        assert_eq!(
            full.iter().map(|slot| slot.offset.0).collect::<Vec<_>>(),
            (0..8).collect::<Vec<_>>()
        );
        module.disk.release_for_test();
        Ok(())
    }

    #[test]
    fn force_read_takes_a_young_short_span() -> eyre::Result<()> {
        let (_tmp, module) = span_fixture("span_force", 8, 8, 1)?;
        {
            let mut queue = module.disk.queue();
            queue.slots.push(sweep_slot(0));
        }
        module.disk.occupy_for_test();
        assert!(module.take_disk_span().is_none());
        let span = module.take_ready_span(true, true, false).expect("forced");
        assert_eq!(span[0].offset.0, 0);
        module.disk.release_for_test();
        Ok(())
    }

    #[test]
    fn take_disk_span_aged_slot_jumps_a_longer_span() -> eyre::Result<()> {
        let (_tmp, module) = span_fixture("span_aged", 8, 8, 1)?;
        {
            let mut queue = module.disk.queue();
            for offset in [0_u32, 3, 4, 5] {
                queue.slots.push(sweep_slot(offset));
            }
            queue.slots[0].queued_at = Instant::now()
                .checked_sub(Duration::from_secs(2))
                .expect("test clock");
        }
        module.disk.occupy_for_test();
        let span = module.take_disk_span().expect("span");
        let offsets: Vec<u32> = span.iter().map(|slot| slot.offset.0).collect();
        assert_eq!(offsets, vec![0]);
        module.disk.release_for_test();
        Ok(())
    }

    fn span_order_fixture() -> eyre::Result<(
        irys_testing_utils::utils::tempfile::TempDir,
        super::super::StorageModule,
    )> {
        span_fixture("span_order", 8, 8, 8)
    }

    fn span_fixture(
        prefix: &str,
        chunks: u64,
        max_chunks: u64,
        hole_chunks: u64,
    ) -> eyre::Result<(
        irys_testing_utils::utils::tempfile::TempDir,
        super::super::StorageModule,
    )> {
        let tmp_dir = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix(prefix)
            .with_tracing()
            .build();
        let chunk_size = 32;
        let last = chunks.saturating_sub(1);
        let node_config = NodeConfig {
            consensus: ConsensusOptions::Custom(ConsensusConfig {
                chunk_size,
                num_chunks_in_partition: chunks,
                ..ConsensusConfig::testing()
            }),
            storage: StorageSyncConfig {
                num_writes_before_sync: 1000,
                max_pending_write_bytes: None,
                entropy_sweep_interval_millis: 60_000,
                entropy_sweep_max_bytes: chunk_size * max_chunks,
                entropy_coalesce_hole_bytes: chunk_size * hole_chunks,
                drop_recall_page_cache: false,
            },
            base_directory: tmp_dir.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        let config = irys_types::Config::new_with_random_peer_id(node_config);
        let module = super::super::StorageModule::new(
            &super::super::StorageModuleInfo {
                id: 0,
                partition_assignment: Some(irys_types::partition::PartitionAssignment::default()),
                submodules: vec![(
                    ii(
                        PartitionChunkOffset::from(0),
                        PartitionChunkOffset::from(u32::try_from(last).expect("span fixture")),
                    ),
                    "chunks".into(),
                )],
            },
            &config,
        )?;
        Ok((tmp_dir, module))
    }
}
