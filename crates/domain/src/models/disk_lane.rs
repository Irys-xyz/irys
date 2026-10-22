//! One thread per storage module locks `chunks.dat`. Order is mining recall,
//! then a ready packed write, then an entropy read. A recall does not cancel
//! a `pwrite` already in the kernel, and this thread submits no new write
//! while `recall_waiters` is set. With `storage.recall_io_priority` false a
//! recall does not hold the disk: `recall_holds` stays clear and every IO
//! that otherwise waits for a recall shares the disk with its preads.
//!
//! Presence rows and packed checksums are written only after this gap's
//! `fdatasync`. `min_durable_interval_ms` can defer that pair. `0`, force,
//! shutdown, drop, and the pending-byte headroom or ceiling do not. Recall
//! does not wait on a deferred pair. `intervals.json` gets an `Interrupted`
//! mark before the `pwrite`s. A completion is rewritten with the next
//! write-ahead, on the persist interval, or on shutdown.
//!
//! Reset, a partition-hash change, `pause_data_writes`, and a write at the
//! offset drop cached read-ahead pads. The generation has to match.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs::File,
    ops::Deref,
    sync::{
        Arc, Condvar, Mutex, MutexGuard, PoisonError, TryLockError,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use irys_database::submodule::SubmoduleStore as _;
use irys_packing::packing_xor_vec_u8;
use irys_types::{ChunkDataPath, ChunkPathHash, PartitionChunkOffset, UnpackedChunk};
use nodit::Interval;
use std::os::unix::fs::FileExt as _;

use super::{BatchEnqueueItem, ChunkType, StorageModule, WriteDataChunkError, WriteRun};

/// How long a caller waits for its entropy read before it gives up.
const SWEEP_WAIT: Duration = Duration::from_secs(30);
/// How long the lane waits after a packed write fails before it tries the
/// same bytes again. The bytes stay queued. Shutdown and `force_sync` write
/// immediately and do not use this wait.
const WRITE_RETRY_BACKOFF: Duration = Duration::from_secs(1);
/// No-lane packed flush still commits at most this many runs.
const INFLIGHT_DISK_OPS: usize = 3;
/// Commands one storage module may have in the kernel at once. This is the
/// block-queue depth. The service-time budget can stop earlier.
const INFLIGHT_WRITES: usize = 64;
/// Pending chunks sorted by offset before one flush. A larger
/// `num_writes_before_sync` widens the scan so later chunks are still joined.
pub(super) const WRITE_REORDER_CHUNKS: usize = 1_000;

/// Chunks looked at before one flush. At least [`WRITE_REORDER_CHUNKS`], and
/// at least the durability count.
pub(super) fn write_reorder_scan(num_writes_before_sync: u64) -> usize {
    WRITE_REORDER_CHUNKS.max(usize::try_from(num_writes_before_sync).unwrap_or(usize::MAX))
}
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

/// Wait before a short run is eligible while the chunk disk is busy.
///
/// One capped run is too short: a neighbor would miss the flush. This is the
/// service time of one default reorder scan of capped runs.
pub(super) fn reorder_grace(chunk_size: u64) -> Duration {
    let chunk = chunk_size.max(1);
    let per_run = (WRITE_RUN_MAX_BYTES / chunk).max(1);
    let runs = (WRITE_REORDER_CHUNKS as u64 / per_run).max(1);
    let run_bytes = WRITE_RUN_MAX_BYTES.min(per_run.saturating_mul(chunk));
    Duration::from_micros(service_us(runs, runs.saturating_mul(run_bytes)))
}

type SweepNotify = tokio::sync::oneshot::Sender<Result<(), WriteDataChunkError>>;

/// Condvar shared by the lane and recall waiters.
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
    /// When this slot joined the sweep queue. A short span waits from the
    /// oldest slot, so a neighbor does not restart the grace.
    enqueued_at: Instant,
    /// Entropy taken from the read-ahead cache. Not pending-map entropy:
    /// the interval is checked again before the XOR.
    cached_entropy: Option<Vec<u8>>,
    /// `entropy_write_epoch` when these entropy bytes were copied. A later
    /// entropy pwrite bumps that epoch. Packing this buffer after the bump
    /// stores the old bytes as data.
    entropy_epoch: u64,
}

/// One reserved offset. The third field is entropy already in `pending`.
/// The fourth is a read-ahead cache pad. Only one of them is set.
/// The fifth is `entropy_write_epoch` when those bytes were copied. `0` when
/// the bytes are still on disk and this slot will pread them.
type SelectedOffset = (
    PartitionChunkOffset,
    u64,
    Option<Vec<u8>>,
    Option<Vec<u8>>,
    u64,
);

/// Read-ahead pads past a demand span. Oldest live pad is evicted first.
///
/// `order` is a FIFO of `(offset, stamp)`. A hit leaves the FIFO row. Eviction
/// skips a stale stamp, so a remove does not scan the FIFO.
#[derive(Debug, Default)]
pub(super) struct EntropyPadCache {
    order: std::collections::VecDeque<(PartitionChunkOffset, u64)>,
    pads: HashMap<PartitionChunkOffset, CachedPad>,
    bytes: u64,
    next_stamp: u64,
}

#[derive(Debug)]
struct CachedPad {
    bytes: Vec<u8>,
    generation: u64,
    stamp: u64,
    /// `entropy_write_epoch` of the pread that stored `bytes`.
    entropy_epoch: u64,
}

impl EntropyPadCache {
    pub(super) fn clear(&mut self) {
        self.order.clear();
        self.pads.clear();
        self.bytes = 0;
    }

    pub(super) fn invalidate(&mut self, offset: PartitionChunkOffset) {
        self.remove(offset);
    }

    pub(super) fn invalidate_range(
        &mut self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) {
        let offsets: Vec<_> = self
            .pads
            .keys()
            .copied()
            .filter(|offset| *offset >= start && *offset <= end)
            .collect();
        for offset in offsets {
            self.invalidate(offset);
        }
    }

    fn remove(&mut self, offset: PartitionChunkOffset) -> Option<CachedPad> {
        let pad = self.pads.remove(&offset)?;
        self.bytes = self.bytes.saturating_sub(pad.bytes.len() as u64);
        // One stale FIFO row per remove. Compact before that list can grow
        // without bound across a long run of hits.
        if self.order.len() > self.pads.len().saturating_mul(2).saturating_add(16) {
            self.compact_order();
        }
        Some(pad)
    }

    fn compact_order(&mut self) {
        let pads = &self.pads;
        self.order
            .retain(|(offset, stamp)| pads.get(offset).is_some_and(|pad| pad.stamp == *stamp));
    }

    /// Drop the oldest pad that is still stored. Stale FIFO rows are skipped.
    fn evict_oldest(&mut self) -> bool {
        while let Some((offset, stamp)) = self.order.pop_front() {
            let Some(pad) = self.pads.get(&offset) else {
                continue;
            };
            if pad.stamp != stamp {
                continue;
            }
            let pad = self.pads.remove(&offset).expect("stamp matched");
            self.bytes = self.bytes.saturating_sub(pad.bytes.len() as u64);
            return true;
        }
        false
    }

    /// Drop the pad. `Some` only when the generation still matches.
    /// The epoch is the one stored with the bytes.
    fn take(&mut self, offset: PartitionChunkOffset, generation: u64) -> Option<(Vec<u8>, u64)> {
        let pad = self.remove(offset)?;
        (pad.generation == generation).then_some((pad.bytes, pad.entropy_epoch))
    }

    /// Keep `bytes` for `offset`. Evict the oldest pads until it fits.
    /// A pad larger than `cap` is not stored. Returns whether it was stored.
    fn insert(
        &mut self,
        offset: PartitionChunkOffset,
        bytes: Vec<u8>,
        generation: u64,
        cap: u64,
        entropy_epoch: u64,
    ) -> bool {
        let len = bytes.len() as u64;
        if cap == 0 || len == 0 || len > cap {
            return false;
        }
        self.remove(offset);
        while self.bytes.saturating_add(len) > cap {
            if !self.evict_oldest() {
                break;
            }
        }
        if self.bytes.saturating_add(len) > cap {
            return false;
        }
        self.bytes = self.bytes.saturating_add(len);
        let stamp = self.next_stamp;
        self.next_stamp = self.next_stamp.wrapping_add(1);
        self.order.push_back((offset, stamp));
        self.pads.insert(
            offset,
            CachedPad {
                bytes,
                generation,
                stamp,
                entropy_epoch,
            },
        );
        true
    }
}

/// One body whose entropy targets are reserved and not yet in the sweep queue.
struct OccupiedChunk {
    unpacked: Arc<Vec<u8>>,
    data_path: Arc<Vec<u8>>,
    path_hash: ChunkPathHash,
    priority: WritePriority,
    waited: bool,
    notify: Option<SweepNotify>,
    selected: Vec<SelectedOffset>,
}

enum OccupyFailure {
    Write(WriteDataChunkError),
    /// The offset already has an index write reserved.
    InFlight,
}

#[derive(Default)]
struct SweepQueue {
    /// One slot per offset. Offset order is the span order.
    slots: BTreeMap<PartitionChunkOffset, SweepSlot>,
    /// Slots that already hold entropy. Fold scans this set, not `slots`.
    memory_offsets: BTreeSet<PartitionChunkOffset>,
    /// Live slots at each [`WritePriority`] discriminant.
    priority_slots: [usize; 3],
    /// Memory slots at each priority. A disk backlog does not hide these.
    memory_slots: [usize; 3],
    groups: HashMap<u64, SweepGroup>,
    next_group: u64,
}

fn priority_index(priority: WritePriority) -> usize {
    match priority {
        WritePriority::Migration => 0,
        WritePriority::Packing => 1,
        WritePriority::Ingress => 2,
    }
}

impl SweepQueue {
    fn disk_len(&self) -> usize {
        self.slots.len().saturating_sub(self.memory_offsets.len())
    }

    fn best_priority(&self) -> Option<WritePriority> {
        const ORDER: [WritePriority; 3] = [
            WritePriority::Migration,
            WritePriority::Packing,
            WritePriority::Ingress,
        ];
        ORDER
            .into_iter()
            .find(|priority| self.priority_slots[priority_index(*priority)] > 0)
    }

    /// Returns the slot this offset already held.
    ///
    /// The caller finishes that group and releases its unpacked length.
    /// Dropping the slot leaves the oneshot unfinished and the length in
    /// `queued_unpacked_bytes`.
    fn insert_slot(&mut self, slot: SweepSlot) -> Option<SweepSlot> {
        let offset = slot.offset;
        let displaced = if self.slots.contains_key(&offset) {
            self.remove_slot(offset)
        } else {
            None
        };
        let index = priority_index(slot.priority);
        self.priority_slots[index] = self.priority_slots[index].saturating_add(1);
        if memory_entropy(&slot) {
            self.memory_slots[index] = self.memory_slots[index].saturating_add(1);
            self.memory_offsets.insert(offset);
        }
        self.slots.insert(offset, slot);
        displaced
    }

    fn remove_slot(&mut self, offset: PartitionChunkOffset) -> Option<SweepSlot> {
        let slot = self.slots.remove(&offset)?;
        let index = priority_index(slot.priority);
        self.priority_slots[index] = self.priority_slots[index].saturating_sub(1);
        if self.memory_offsets.remove(&offset) {
            self.memory_slots[index] = self.memory_slots[index].saturating_sub(1);
        }
        Some(slot)
    }
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
    /// `storage.recall_io_priority`. When false, a recall does not hold the
    /// disk and other IO does not wait for it.
    recall_io_priority: bool,
    wake: DiskWake,
    queue: Mutex<SweepQueue>,
    sweep_busy: Mutex<()>,
    /// The service thread owns sweeps and flushes while this is set.
    lane_started: AtomicBool,
    lane_stop: AtomicBool,
    /// Set for the life of `run_disk_lane`. External flushes use it to avoid
    /// waiting for themselves.
    lane_thread: Mutex<Option<std::thread::ThreadId>>,
    /// External flush and interval writes wait here. The lane thread runs them.
    handoff: LaneHandoff,
    /// Instant when a failed packed write may be tried again. `None` after a
    /// successful commit, and when no commit has failed.
    write_retry_at: Mutex<Option<Instant>>,
    #[cfg(test)]
    last_commit_thread: Mutex<Option<std::thread::ThreadId>>,
    #[cfg(any(test, feature = "bench"))]
    pub(super) entropy_preads: AtomicU64,
    /// Bytes of entropy preads issued. One span is one pread, holes included.
    #[cfg(any(test, feature = "bench"))]
    pub(super) entropy_pread_bytes: AtomicU64,
    /// Queued chunks served by those preads. Hole bytes are not slots.
    #[cfg(any(test, feature = "bench"))]
    pub(super) entropy_pread_slots: AtomicU64,
    /// Read-ahead pads copied into a sweep slot.
    #[cfg(any(test, feature = "bench"))]
    pub(super) entropy_readahead_hits: AtomicU64,
    /// Read-ahead pads stored after a pread.
    #[cfg(any(test, feature = "bench"))]
    pub(super) entropy_readahead_stores: AtomicU64,
    /// Non-recall `pread`s from `read_chunks`. One contiguous disk run is one
    /// call when the run fits the recall range.
    #[cfg(test)]
    pub(super) source_preads: AtomicU64,
    /// Sweep-queue publishes. One contiguous run is one publish.
    #[cfg(test)]
    pub(super) enqueue_notifies: AtomicU64,
    /// Disk slots a span walk has examined. A rebuild of every span counts
    /// the whole backlog on every take.
    #[cfg(test)]
    pub(super) span_walk_slots: AtomicU64,
    /// One armed pause in the entropy-copy windows. Production builds omit it.
    #[cfg(any(test, feature = "test-utils"))]
    entropy_pack_pause: super::PauseGate<EntropyPackPause>,
    /// Instants `finish_span_read` uses as the pread return, one per span.
    #[cfg(test)]
    span_read_done: Mutex<Vec<(u32, Instant)>>,
    /// Instant `read_exact_at` returned, taken before `read_done`.
    /// A stamp from before that return is earlier than this.
    #[cfg(test)]
    span_pread_returned: Mutex<Vec<(u32, Instant)>>,
    /// Ingress stages recorded for this module. The histogram has no test reader.
    #[cfg(test)]
    ingest_stages: Mutex<Vec<(&'static str, Duration)>>,
    /// Holds each inflight pread after `read_exact_at` returns.
    #[cfg(test)]
    inflight_pread_hold: InflightPreadHold,
    /// Replaces `SWEEP_WAIT` inside `drain_entropy_queue` when set.
    #[cfg(test)]
    sweep_wait_override: Mutex<Option<Duration>>,
    /// When set, every sweep wait bumps the wake epoch before it checks it.
    /// A second thread can miss that window. This lands the wake after `seen`
    /// was sampled and before the deadline check.
    #[cfg(test)]
    notify_on_wait: AtomicBool,
    /// Commits that reached the `pwrite` section. An empty batch does not count.
    #[cfg(test)]
    commit_calls: AtomicU64,
    /// Remaining injected `pwrite` failures. Each taken failure is one `EIO`.
    #[cfg(test)]
    pwrite_fail_remaining: AtomicU64,
}

/// Test barrier around a copied entropy pad. The slot is not in the sweep
/// queue at either point, so `fail_sweep_offset` cannot see it.
#[cfg(any(test, feature = "test-utils"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EntropyPackPause {
    /// The pad or pending entropy is copied. `push_occupied` has not run.
    AfterEntropyCopied,
    /// The pre-XOR check passed. The XOR has not run.
    BeforePackXor,
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
    /// `entropy_write_epoch` captured before this pread.
    entropy_epoch: u64,
    /// When `read_exact_at` returned. Later spans must not inherit this.
    read_done: Instant,
}

/// Test hold after an inflight entropy pread returns. Production builds omit it.
#[cfg(test)]
struct InflightPreadHold {
    armed: Mutex<bool>,
    arrived: Mutex<usize>,
    arrived_cv: Condvar,
    release: Mutex<bool>,
    release_cv: Condvar,
}

#[cfg(test)]
impl InflightPreadHold {
    fn new() -> Self {
        Self {
            armed: Mutex::new(false),
            arrived: Mutex::new(0),
            arrived_cv: Condvar::new(),
            release: Mutex::new(false),
            release_cv: Condvar::new(),
        }
    }

    fn arm(&self) {
        *self.release.lock().unwrap_or_else(PoisonError::into_inner) = false;
        *self.arrived.lock().unwrap_or_else(PoisonError::into_inner) = 0;
        *self.armed.lock().unwrap_or_else(PoisonError::into_inner) = true;
    }

    fn wait_after_pread(&self) {
        if !*self.armed.lock().unwrap_or_else(PoisonError::into_inner) {
            return;
        }
        {
            let mut arrived = self.arrived.lock().unwrap_or_else(PoisonError::into_inner);
            *arrived = arrived.saturating_add(1);
            self.arrived_cv.notify_all();
        }
        let mut release = self.release.lock().unwrap_or_else(PoisonError::into_inner);
        while !*release {
            release = self
                .release_cv
                .wait(release)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn wait_arrived(&self, count: usize, timeout: Duration) -> bool {
        let guard = self.arrived.lock().unwrap_or_else(PoisonError::into_inner);
        let (guard, _) = self
            .arrived_cv
            .wait_timeout_while(guard, timeout, |arrived| *arrived < count)
            .unwrap_or_else(PoisonError::into_inner);
        *guard >= count
    }

    fn release(&self) {
        *self.release.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.release_cv.notify_all();
    }
}

pub(super) struct WriteHold<'a> {
    gate: &'a DiskGate,
}

/// Drops the recall count even when the read returns with an error.
pub(super) struct RecallHold<'a> {
    gate: &'a DiskGate,
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
    pub(super) fn new(recall_io_priority: bool) -> Self {
        Self {
            recall_waiters: AtomicUsize::new(0),
            write_holds: AtomicUsize::new(0),
            disk_ops: AtomicUsize::new(0),
            inflight_bytes: AtomicU64::new(0),
            io_exclusive: AtomicUsize::new(0),
            recall_flush_owed: AtomicBool::new(false),
            recall_io_priority,
            wake: DiskWake::new(),
            queue: Mutex::new(SweepQueue::default()),
            sweep_busy: Mutex::new(()),
            lane_started: AtomicBool::new(false),
            lane_stop: AtomicBool::new(false),
            lane_thread: Mutex::new(None),
            handoff: LaneHandoff::new(),
            write_retry_at: Mutex::new(None),
            #[cfg(test)]
            last_commit_thread: Mutex::new(None),
            #[cfg(any(test, feature = "bench"))]
            entropy_preads: AtomicU64::new(0),
            #[cfg(any(test, feature = "bench"))]
            entropy_pread_bytes: AtomicU64::new(0),
            #[cfg(any(test, feature = "bench"))]
            entropy_pread_slots: AtomicU64::new(0),
            #[cfg(any(test, feature = "bench"))]
            entropy_readahead_hits: AtomicU64::new(0),
            #[cfg(any(test, feature = "bench"))]
            entropy_readahead_stores: AtomicU64::new(0),
            #[cfg(test)]
            source_preads: AtomicU64::new(0),
            #[cfg(test)]
            enqueue_notifies: AtomicU64::new(0),
            #[cfg(test)]
            span_walk_slots: AtomicU64::new(0),
            #[cfg(any(test, feature = "test-utils"))]
            entropy_pack_pause: super::PauseGate::new(),
            #[cfg(test)]
            span_read_done: Mutex::new(Vec::new()),
            #[cfg(test)]
            span_pread_returned: Mutex::new(Vec::new()),
            #[cfg(test)]
            ingest_stages: Mutex::new(Vec::new()),
            #[cfg(test)]
            inflight_pread_hold: InflightPreadHold::new(),
            #[cfg(test)]
            sweep_wait_override: Mutex::new(None),
            #[cfg(test)]
            notify_on_wait: AtomicBool::new(false),
            #[cfg(test)]
            commit_calls: AtomicU64::new(0),
            #[cfg(test)]
            pwrite_fail_remaining: AtomicU64::new(0),
        }
    }

    /// Park the next occupy after the offset is reserved. The sweep has not
    /// taken the slot, so `has_in_flight_index_for` stays true.
    #[cfg(any(test, feature = "test-utils"))]
    pub(super) fn hold_after_entropy_copied(&self) {
        self.entropy_pack_pause
            .arm(EntropyPackPause::AfterEntropyCopied);
    }

    /// `true` once [`Self::hold_after_entropy_copied`] has stopped a writer.
    #[cfg(any(test, feature = "test-utils"))]
    pub(super) fn after_entropy_copied_arrived(&self, timeout: Duration) -> bool {
        self.entropy_pack_pause.wait_arrived(timeout)
    }

    /// Release whichever entropy-copy pause is armed.
    #[cfg(any(test, feature = "test-utils"))]
    pub(super) fn release_entropy_pack_pause(&self) {
        self.entropy_pack_pause.release();
    }

    /// Test drain bound. Production uses `SWEEP_WAIT`.
    #[cfg(test)]
    pub(super) fn set_sweep_wait_for_test(&self, wait: Duration) {
        *self
            .sweep_wait_override
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(wait);
    }

    #[cfg(test)]
    fn sweep_wait_for_test(&self) -> Option<Duration> {
        *self
            .sweep_wait_override
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The next sweep waits see a wake that arrived after `seen` was sampled.
    #[cfg(test)]
    pub(super) fn set_notify_on_wait_for_test(&self, enabled: bool) {
        self.notify_on_wait.store(enabled, Ordering::SeqCst);
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
        self.commit_calls.fetch_add(1, Ordering::SeqCst);
        let mut guard = self
            .last_commit_thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *guard = Some(std::thread::current().id());
    }

    /// Commits that reached the `pwrite` section.
    #[cfg(test)]
    pub(super) fn commit_calls(&self) -> u64 {
        self.commit_calls.load(Ordering::SeqCst)
    }

    /// The next `count` lane `pwrite`s fail before the kernel write.
    #[cfg(test)]
    pub(super) fn fail_next_pwrites(&self, count: u64) {
        self.pwrite_fail_remaining.store(count, Ordering::SeqCst);
    }

    #[cfg(test)]
    fn take_pwrite_failure(&self) -> bool {
        loop {
            let left = self.pwrite_fail_remaining.load(Ordering::SeqCst);
            if left == 0 {
                return false;
            }
            if self
                .pwrite_fail_remaining
                .compare_exchange(left, left - 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return true;
            }
        }
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

    /// Ask the lane to write the live interval map. `Stopped` means this
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

    /// A mining recall holds the disk for other IO. With
    /// `recall_io_priority` false the recall shares the disk and this stays
    /// clear while its preads run.
    pub(super) fn recall_holds(&self) -> bool {
        self.recall_io_priority && self.recall_waiters.load(Ordering::SeqCst) > 0
    }

    /// Recall, a file holder, and a full service budget all block a new submit.
    fn disk_available(&self) -> bool {
        !self.recall_holds()
            && self.write_holds.load(Ordering::SeqCst) == 0
            && self.io_exclusive.load(Ordering::SeqCst) == 0
            && self.disk_ops.load(Ordering::SeqCst) < INFLIGHT_WRITES
            && self.budget_allows(self.disk_ops.load(Ordering::SeqCst), 0)
    }

    /// No lane `pread` or `pwrite` is in the kernel, and no syscall holds the file.
    /// Room left in the service budget is still busy.
    pub(super) fn no_chunk_command(&self) -> bool {
        self.disk_ops.load(Ordering::SeqCst) == 0 && self.io_exclusive.load(Ordering::SeqCst) == 0
    }

    /// No recall hold, write hold, exclusive IO, or in-kernel chunk command.
    /// A short packed write waits for a neighbor only while this is false.
    /// A short entropy span may wait while this is true.
    pub(super) fn chunk_disk_idle(&self) -> bool {
        self.disk_ops.load(Ordering::SeqCst) == 0
            && !self.recall_holds()
            && self.write_holds.load(Ordering::SeqCst) == 0
            && self.io_exclusive.load(Ordering::SeqCst) == 0
    }

    /// Time left before a failed packed write may be tried again.
    /// `None` when no retry is waiting, or the deadline has already passed.
    /// Zero is not pending: the lane wait drops a zero timeout and then
    /// waits with no timeout, so an expired deadline must look unset.
    pub(super) fn write_retry_remaining(&self) -> Option<Duration> {
        let at = {
            let guard = self
                .write_retry_at
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            (*guard)?
        };
        let remain = at.saturating_duration_since(Instant::now());
        if remain.is_zero() { None } else { Some(remain) }
    }

    /// True while [`Self::write_retry_remaining`] is still in the future.
    pub(super) fn write_retry_pending(&self) -> bool {
        self.write_retry_remaining().is_some()
    }

    fn arm_write_retry(&self) {
        let at = Instant::now() + WRITE_RETRY_BACKOFF;
        *self
            .write_retry_at
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(at);
    }

    fn clear_write_retry(&self) {
        *self
            .write_retry_at
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
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
        let mut guard = self.lock_wake();
        // Do not count this recall yet. The owed window has to submit, and a
        // recall waiter would make that submit wait for this recall.
        while self.recall_flush_owed.load(Ordering::SeqCst) {
            guard = self.wake.wait(guard);
        }
        self.recall_waiters.fetch_add(1, Ordering::SeqCst);
        self.wake.notify_locked();
        drop(guard);
        RecallHold { gate: self }
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
        self.recall_holds()
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

    /// Block until `next_bytes` can be submitted, or a recall holds the disk.
    /// The check and the wait share the wake lock.
    fn wait_for_write_slot(&self, next_bytes: u64) {
        let mut guard = self.lock_wake();
        while !self.recall_holds() && self.submit_blocked(INFLIGHT_WRITES, false, next_bytes) {
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
    /// A recall that does not hold the disk is not waited on.
    pub(super) fn yield_to_recall(&self) {
        if !self.recall_io_priority {
            return;
        }
        let mut guard = self.lock_wake();
        while self.recall_waiters.load(Ordering::SeqCst) > 0 {
            guard = self.wake.wait(guard);
        }
    }

    /// Wait until mining recall is clear, or until `deadline`.
    /// Returns false when the deadline passes while a recall still holds the disk.
    /// A recall that does not hold the disk is not waited on.
    pub(super) fn yield_to_recall_until(&self, deadline: Instant) -> bool {
        if !self.recall_io_priority {
            return true;
        }
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

    fn writes_active(&self) -> bool {
        self.write_holds.load(Ordering::SeqCst) > 0
    }

    fn queue(&self) -> std::sync::MutexGuard<'_, SweepQueue> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    #[cfg(feature = "bench")]
    pub(super) fn bench_sweep_slots(&self) -> usize {
        self.queue().slots.len()
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
    /// Mining recall. Announced before `chunks.dat` is locked, so a sweep or
    /// write that has not taken the file yet yields. The `pread`s stay on this
    /// caller and overlap `pwrite`s already in the kernel. The lane submits no
    /// new write while the hold is set.
    pub fn read_recall_chunks(
        &self,
        chunk_range: Interval<PartitionChunkOffset>,
    ) -> eyre::Result<super::RecallChunks> {
        let (chunks, rows) = {
            let _recall = self.disk.begin_recall();
            self.read_chunks_inner(chunk_range, true, true)?
        };
        if let Err(error) = self.flush_after_recall() {
            tracing::error!(
                "Couldn't flush packed chunks after recall for storage_module {}: {error}",
                self.id
            );
        }
        // Entropy stays behind the packed window. The lane sweeps itself.
        // Do not confirm checksum suspects on this thread: a durable point
        // holds `sync_in_progress` inside `yield_to_recall`, so taking that
        // lock under the recall deadlocks. With no lane there is no service
        // tick, so confirm once the hold has dropped.
        if !self.disk.lane_running() {
            self.pump_entropy_reads();
            self.confirm_crc_suspects();
        }
        Ok(super::RecallChunks { chunks, rows })
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
        if self.disk.recall_holds() || self.disk.writes_active() {
            return;
        }
        self.fold_memory_entropy();
        if self.disk.lane_running() {
            self.sweep_inflight();
        } else {
            self.sweep_one();
        }
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
            Err(OccupyFailure::InFlight) => Err(WriteDataChunkError::Backpressure),
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
                        .map(|(offset, ..)| *offset)
                        .collect();
                    held.push(occupied);
                    items.push(BatchEnqueueItem::Queued(offsets));
                }
                Ok(None) => items.push(BatchEnqueueItem::NotQueued),
                // Nothing was reserved. The caller retries this body.
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
        #[cfg(test)]
        self.wait_if_sync_paused(super::SyncPausePoint::AfterOccupyResolved);

        let data_path = Arc::new(chunk.data_path.0.clone());
        let path_hash = UnpackedChunk::hash_data_path(&data_path);
        let unpacked = Arc::new(chunk.bytes.0.clone());

        // Entropy on disk, read before `pending`. A queued entropy write is
        // still a target when the interval has not flipped yet.
        let disk_entropy: HashSet<PartitionChunkOffset> = {
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
        let chunk_size = self.config.consensus.chunk_size;
        let num_chunks_in_partition = self.config.consensus.num_chunks_in_partition;

        let mut selected: Vec<SelectedOffset> = Vec::new();
        {
            let mut pending = self.pending_writes.write().unwrap();
            if self.data_writes_paused() {
                return Err(OccupyFailure::Write(WriteDataChunkError::WritesPaused));
            }
            let generation = self.index_write_generation.load(Ordering::SeqCst);
            // Same funding math as `partition_offsets_for_data_root_chunk`,
            // read again under this lock. It uses data-root rows only, so a
            // short test path is not parsed. Reset clears those rows before
            // a later entropy write can take `pending`.
            let still_funded = (|| -> eyre::Result<HashSet<PartitionChunkOffset>> {
                let raw_tx_offset = u32::from(chunk.tx_offset);
                let chunk_byte_offset = u64::from(raw_tx_offset)
                    .checked_mul(chunk_size)
                    .ok_or_else(|| eyre::eyre!("chunk byte offset overflow"))?;
                let mut funded = HashSet::new();
                for (_, submodule) in self.submodules.iter() {
                    let Some(infos) = submodule
                        .db
                        .view(|tx| tx.get_data_root_infos_for_data_root(chunk.data_root))?
                    else {
                        continue;
                    };
                    for info in infos.0 {
                        if chunk_byte_offset >= info.data_size {
                            continue;
                        }
                        let relative_offset =
                            i64::from(info.start_offset.0) + i64::from(raw_tx_offset);
                        if relative_offset < 0 || relative_offset as u64 >= num_chunks_in_partition
                        {
                            continue;
                        }
                        let relative_offset = u32::try_from(relative_offset).map_err(|_| {
                            eyre::eyre!("partition-relative chunk offset exceeds u32")
                        })?;
                        funded.insert(PartitionChunkOffset::from(relative_offset));
                    }
                }
                Ok(funded)
            })();
            let still_funded = match still_funded {
                Ok(funded) => funded,
                Err(error) => {
                    return Err(OccupyFailure::Write(WriteDataChunkError::Other(error)));
                }
            };
            // A busy placement blocks the whole occupy. Writing the free
            // placements and returning Ok lets ingress accept the chunk while
            // the busy one can still fail and stay empty. Decide here, before
            // any reserve, so the holder cannot release the offset in a gap.
            let mut saw_occupancy = false;
            let mut planned: Vec<(PartitionChunkOffset, Option<Vec<u8>>)> = Vec::new();
            for partition_offset in &partition_offsets {
                let partition_offset = *partition_offset;
                if pending.occupancy.contains_key(&partition_offset) {
                    saw_occupancy = true;
                    continue;
                }
                if pending
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
                if !still_funded.contains(&partition_offset) {
                    continue;
                }
                planned.push((partition_offset, pending_entropy));
            }
            if saw_occupancy {
                return Err(OccupyFailure::InFlight);
            }
            for (partition_offset, pending_entropy) in planned {
                pending.occupancy.insert(partition_offset, generation);
                // A cache pad is disk entropy, not a pending write. Take it
                // under this lock so a write cannot invalidate between the
                // check and the take. Keep the epoch stored with the pad.
                // The bump is published before `stamp_pwrite` invalidates the
                // cache, so a sample under this lock can see the new epoch
                // and the old bytes.
                let taken = if pending_entropy.is_none() {
                    self.entropy_cache
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .take(partition_offset, generation)
                } else {
                    None
                };
                let entropy_epoch = taken.as_ref().map(|(_, epoch)| *epoch).unwrap_or(0);
                let cached_entropy = taken.map(|(bytes, _)| bytes);
                // The hit is counted when this pad is packed. A rejected pad
                // reads `chunks.dat` on the retry, so the take is not a hit.
                selected.push((
                    partition_offset,
                    generation,
                    pending_entropy,
                    cached_entropy,
                    entropy_epoch,
                ));
            }
            if selected.is_empty() {
                return Ok(None);
            }
            let queued = unpacked.len() as u64 * selected.len() as u64;
            pending.queued_unpacked_bytes = pending.queued_unpacked_bytes.saturating_add(queued);
        }

        // Not queued yet. A newer entropy write can durable-point in this gap.
        #[cfg(any(test, feature = "test-utils"))]
        self.disk
            .entropy_pack_pause
            .wait_if(EntropyPackPause::AfterEntropyCopied);

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
                    .any(|(_, generation, _, _, _)| *generation != generation_now)
            });
        if stale {
            drop(queue);
            self.release_occupied(occupied);
            return Err(WriteDataChunkError::WritesPaused);
        }
        let mut groups = Vec::with_capacity(occupied.len());
        let mut displaced = Vec::new();
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
            let enqueued_at = Instant::now();
            for (offset, generation, pending_entropy, cached_entropy, entropy_epoch) in
                chunk.selected
            {
                if let Some(previous) = queue.insert_slot(SweepSlot {
                    group: group_id,
                    offset,
                    unpacked: Arc::clone(&chunk.unpacked),
                    data_path: Arc::clone(&chunk.data_path),
                    path_hash: chunk.path_hash,
                    generation,
                    priority: chunk.priority,
                    pending_entropy,
                    cached_entropy,
                    entropy_epoch,
                    byte_len,
                    enqueued_at,
                }) {
                    displaced.push(previous);
                }
            }
            groups.push(group_id);
        }
        drop(queue);
        self.retire_displaced_slots(displaced);
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
            for (offset, generation, ..) in chunk.selected {
                self.release_queued_offset(offset, generation, byte_len);
            }
        }
    }

    pub(super) fn drive_group(&self, group: u64) -> Result<(), WriteDataChunkError> {
        let mut deadline = Instant::now() + SWEEP_WAIT;
        loop {
            if let Some(result) = self.take_group_result(group) {
                return result;
            }
            // Recall can stay queued for the whole step. Stop this waiter when
            // the monotonic deadline passes, so the wait does not run until
            // recall goes idle. A group that finished keeps that result.
            if Instant::now() >= deadline || !self.disk.yield_to_recall_until(deadline) {
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
            // A short span waits out `entropy_read_grace_ms`, including while
            // the disk is idle. Wake then, so this caller retries when the
            // span becomes eligible. The sweep give-up still bounds the wait.
            let seen = self.disk.wake.epoch();
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
        while let Some(remain) = deadline.checked_duration_since(Instant::now()) {
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
        // `Receiver` is `Unpin`. A timeout on `&mut rx` drops the borrow, not
        // the receiver, so a result sent as the deadline passes is still
        // available to `try_recv`.
        match tokio::time::timeout(SWEEP_WAIT, &mut rx).await {
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
            if self.entropy_idle() {
                return;
            }
            let overall =
                *idle_deadline.get_or_insert_with(|| Instant::now() + self.sweep_give_up());
            // A recall that stays up never reaches the wait below. The same
            // give-up ends this hold. A wake after the give-up still ends it:
            // the epoch check alone would keep looping while notifies arrive.
            let yielded = self.disk.yield_to_recall_until(overall);
            if !yielded || Instant::now() >= overall {
                tracing::warn!("entropy queue did not drain");
                return;
            }
            let folded = self.fold_memory_entropy();
            let swept = self.sweep_one();
            if folded || swept {
                idle_deadline = None;
                continue;
            }
            // Every disk span is waiting on a write, including one that
            // joins a parked offset to a readable neighbor. `None` is that
            // wait. `Some(0)` is eligible and `sweep_one` issues it.
            // A memory slot at the best priority can still fold. The caller
            // commits the parked write after this return.
            let spans_wait_on_writes = self.read_grace_remaining().is_none() && {
                let queue = self.disk.queue();
                queue.disk_len() > 0
            };
            if spans_wait_on_writes && !self.memory_entropy_can_fold() {
                return;
            }
            let seen = self.disk.wake.epoch();
            if self.entropy_idle() {
                return;
            }
            // A young short span is not eligible yet. Sleep only until the
            // grace ends, then issue it. The sweep give-up is still the bound
            // when the span never becomes eligible.
            let wake_by = self
                .read_grace_remaining()
                .filter(|delay| !delay.is_zero())
                .and_then(|delay| Instant::now().checked_add(delay))
                .map(|at| at.min(overall))
                .unwrap_or(overall);
            self.wait_until_epoch_advances(seen, wake_by);
            if Instant::now() >= overall {
                tracing::warn!("entropy queue did not drain");
                return;
            }
        }
    }

    /// Pack a reread that was blocked on rows a force flush just committed,
    /// then make that body durable. One pass. A write that lands during the
    /// pass can block the slot again. An empty queue does not sync again.
    pub(super) fn sync_queued_after_force(&self) -> eyre::Result<()> {
        if self.entropy_idle() {
            return Ok(());
        }
        self.drain_entropy_queue();
        if !self.has_pending_writes() {
            return Ok(());
        }
        self.sync_pending_chunks_inner(true)
    }

    /// Bound used by `drain_entropy_queue`. Tests shorten it. Production
    /// stays at `SWEEP_WAIT`.
    fn sweep_give_up(&self) -> Duration {
        #[cfg(test)]
        if let Some(wait) = self.disk.sweep_wait_for_test() {
            return wait;
        }
        SWEEP_WAIT
    }

    /// Sleep until `epoch` moves past `seen`, or until `deadline`.
    /// Returns true when the deadline passes first. The check and the wait
    /// share the wake lock, so a completion cannot land between them.
    fn wait_until_epoch_advances(&self, seen: u64, deadline: Instant) -> bool {
        let mut guard = self.disk.lock_wake();
        loop {
            #[cfg(test)]
            if self.disk.notify_on_wait.load(Ordering::SeqCst) {
                self.disk.wake.notify_locked();
            }
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

    /// Fail a queued sweep slot at `offset`. The copied pad is stale.
    /// One map lookup. A scan of the backlog on every chunk write tracked
    /// the queue length.
    pub(super) fn fail_sweep_offset(&self, offset: PartitionChunkOffset) {
        let removed = {
            let mut queue = self.disk.queue();
            queue.remove_slot(offset)
        };
        let Some(slot) = removed else {
            return;
        };
        // The pad was copied before this write. A waiter retries. A batch
        // caller already returned, so the slot is read from disk after the
        // write lands instead of dropping the chunk.
        let _ = self.reject_backpressure(slot, true);
    }

    pub(super) fn cancel_sweep_range(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) {
        let groups: HashSet<u64> = {
            let queue = self.disk.queue();
            queue
                .slots
                .values()
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
        self.disk.queue().slots.is_empty()
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
            let Some(best) = queue.best_priority() else {
                return false;
            };
            if queue.memory_slots[priority_index(best)] == 0 {
                return false;
            }
            let offsets: Vec<_> = queue
                .memory_offsets
                .iter()
                .copied()
                .filter(|offset| {
                    queue
                        .slots
                        .get(offset)
                        .is_some_and(|slot| slot.priority == best)
                })
                .collect();
            let mut ready = Vec::with_capacity(offsets.len());
            for offset in offsets {
                if let Some(slot) = queue.remove_slot(offset) {
                    ready.push(slot);
                }
            }
            ready
        };
        if ready.is_empty() {
            return false;
        }
        let mut deferred = Vec::new();
        let mut packed_any = false;
        for slot in ready {
            if self.disk.recall_holds() {
                deferred.push(slot);
                continue;
            }
            self.pack_one(slot);
            packed_any = true;
        }
        if !deferred.is_empty() {
            self.restore_span(deferred);
        }
        packed_any
    }

    /// One pread, bounded by the sweep cap. Bytes that cover a hole are discarded.
    /// Index submits happen only after every kept offset in the span has been
    /// rechecked, so one bad offset does not leave a sibling committed.
    /// A short span waits out the read grace. Recall and a packed-write hold
    /// still gate it.
    fn sweep_one(&self) -> bool {
        let Some(_busy) = self.disk.try_sweep_busy() else {
            return false;
        };
        if self.disk.recall_holds() || self.disk.writes_active() {
            return false;
        }
        let chunk_size = self.config.consensus.chunk_size.max(1);
        let Some(span) = self.take_ready_span() else {
            return false;
        };
        if self.span_blocked_on_pending(&span) {
            self.restore_span(span);
            return false;
        }
        if self.disk.recall_holds() || self.disk.writes_active() {
            self.restore_span(span);
            return false;
        }

        let start = span[0].offset;
        let read_end = self.issued_read_end(&span);
        let chunk_len = chunk_size as usize;
        let span_chunks = (read_end.0 - start.0 + 1) as usize;
        let (file_arc, file_offset) = match self.locate_io(start, true) {
            Ok((file, offset, _)) => (file, offset),
            Err(error) => {
                let error = WriteDataChunkError::Other(error);
                self.fail_span(span, &error);
                return true;
            }
        };
        let mut buf = vec![0_u8; span_chunks.saturating_mul(chunk_len)];
        // A recall or flush that arrived after the span was taken goes
        // first. Put the span back and let the caller wait, so this sweep
        // does not sit inside a recall that never goes idle.
        if self.disk.writes_active() || self.disk.recall_holds() {
            self.restore_span(span);
            return false;
        }
        let file = self.disk.lock_chunks(&file_arc);
        if self.disk.recall_holds() || self.disk.writes_active() {
            drop(file);
            self.restore_span(span);
            return false;
        }
        self.note_issued_entropy_pread(span.len(), buf.len() as u64);
        if let Err(error) = file.read_exact_at(&mut buf, file_offset) {
            drop(file);
            let error = WriteDataChunkError::Other(eyre::eyre!(
                "entropy sweep at offset {start} count {span_chunks}: {error}"
            ));
            self.fail_span(span, &error);
            return true;
        }
        // The pread has returned. A pause or a later span's XOR is not part
        // of this span's queue time.
        let read_done = Instant::now();
        // Captured before the file lock drops. A later entropy pwrite needs
        // that lock, so it cannot land and then hide behind this value.
        let entropy_epoch = self.entropy_write_epoch.load(Ordering::SeqCst);
        drop(file);
        #[cfg(test)]
        self.wait_if_sync_paused(super::SyncPausePoint::AfterEntropyPread);
        self.finish_span_read(span, start, &buf, entropy_epoch, read_done);
        true
    }

    /// Entropy preads up to the service budget, in offset order. A short span
    /// is included once it is past the read grace or it already meets the
    /// batch target. Recall, a packed-write hold, and the in-flight budget
    /// still stop the pass. The lane thread XORs and submits the index only
    /// after those calls return.
    fn sweep_inflight(&self) {
        if self.disk.recall_holds() || self.disk.writes_active() {
            return;
        }
        let chunk_size = self.config.consensus.chunk_size.max(1);
        let mut spans = Vec::new();
        let mut read_ends = Vec::new();
        let mut guards = Vec::with_capacity(INFLIGHT_WRITES);
        while spans.len() < INFLIGHT_WRITES {
            if self.disk.recall_holds() || self.disk.writes_active() {
                break;
            }
            let Some(span) = self.take_ready_span() else {
                break;
            };
            if self.span_blocked_on_pending(&span) {
                self.restore_span(span);
                break;
            }
            // Only the first span of a pass may read ahead. A later span
            // means other chunk work was already queued.
            let read_end = if spans.is_empty() {
                self.issued_read_end(&span)
            } else {
                span[span.len() - 1].offset
            };
            let bytes = span_command_bytes(span[0].offset.0, read_end.0, chunk_size);
            if !self.disk.try_submit_read(INFLIGHT_WRITES, bytes) {
                self.restore_span(span);
                break;
            }
            spans.push(span);
            read_ends.push(read_end);
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
            for ((span, read_end), guard) in spans.iter().zip(read_ends).zip(guards) {
                handles.push(scope.spawn(move || {
                    let _guard = guard;
                    self.read_span_inflight(span, read_end)
                }));
            }
            handles
                .into_iter()
                .map(|handle| handle.join().expect("entropy pread"))
                .collect::<Vec<_>>()
        });
        for (span, result) in spans.into_iter().zip(results) {
            match result {
                Ok(load) => self.finish_span_read(
                    span,
                    load.start,
                    &load.buf,
                    load.entropy_epoch,
                    load.read_done,
                ),
                Err(error) => self.fail_span(span, &error),
            }
        }
    }

    fn read_span_inflight(
        &self,
        span: &[SweepSlot],
        read_end: PartitionChunkOffset,
    ) -> Result<SpanLoad, WriteDataChunkError> {
        let start = span[0].offset;
        let chunk_size = self.config.consensus.chunk_size.max(1);
        let chunk_len = chunk_size as usize;
        let span_chunks = (read_end.0 - start.0 + 1) as usize;
        let (file, file_offset) = self
            .locate_io(start, true)
            .map_err(WriteDataChunkError::Other)
            .map(|(file, offset, _)| (file, offset))?;
        let mut buf = vec![0_u8; span_chunks.saturating_mul(chunk_len)];
        let cloned = {
            let guard = file.lock().unwrap_or_else(PoisonError::into_inner);
            guard.try_clone().map_err(|error| {
                WriteDataChunkError::Other(eyre::eyre!(
                    "entropy sweep at offset {start} count {span_chunks}: {error}"
                ))
            })?
        };
        self.note_issued_entropy_pread(span.len(), buf.len() as u64);
        // The cloned fd does not hold the chunk-file lock. Capture the epoch
        // before the read so a pwrite that lands during it is visible.
        let entropy_epoch = self.entropy_write_epoch.load(Ordering::SeqCst);
        cloned
            .read_exact_at(&mut buf, file_offset)
            .map_err(|error| {
                WriteDataChunkError::Other(eyre::eyre!(
                    "entropy sweep at offset {start} count {span_chunks}: {error}"
                ))
            })?;
        // The test note is this pread's return. `read_done` is after that
        // note and before the test hold. The hold is after the pread, and
        // the join waits for every span, so a stamp here is this span's return.
        #[cfg(test)]
        self.disk
            .span_pread_returned
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((start.0, Instant::now()));
        let read_done = Instant::now();
        #[cfg(test)]
        self.disk.inflight_pread_hold.wait_after_pread();
        Ok(SpanLoad {
            start,
            buf,
            entropy_epoch,
            read_done,
        })
    }

    fn finish_span_read(
        &self,
        span: Vec<SweepSlot>,
        start: PartitionChunkOffset,
        buf: &[u8],
        entropy_epoch: u64,
        read_done: Instant,
    ) {
        #[cfg(test)]
        self.disk
            .span_read_done
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((start.0, read_done));
        let chunk_len = self.config.consensus.chunk_size.max(1) as usize;
        let mut failed_groups = std::collections::HashSet::new();
        let mut accepted = Vec::new();
        for mut slot in span {
            slot.entropy_epoch = entropy_epoch;
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
            let start_index = (slot.offset.0 - start.0) as usize * chunk_len;
            let entropy = buf[start_index..start_index + chunk_len].to_vec();
            if let Err(error) = self.slot_still_valid(&slot, Some(&entropy)) {
                if matches!(error, WriteDataChunkError::Backpressure) {
                    if !self.reject_backpressure(slot, true) {
                        failed_groups.insert(group);
                    }
                } else {
                    failed_groups.insert(group);
                    self.fail_slot(slot, error);
                    self.fail_queued_group(group);
                }
                continue;
            }
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
        // The sweep reads packed entropy and then XORs it. A bad source must
        // not be packed into a data chunk. The checksum is the offset row
        // committed with those bytes.
        let verdicts = match self.crc_span_verdicts(start, buf) {
            Ok(verdicts) => verdicts,
            Err(error) => {
                let error = WriteDataChunkError::Other(error);
                for (slot, _) in pending_submit {
                    let group = slot.group;
                    self.fail_slot(slot, clone_error(&error));
                    self.fail_queued_group(group);
                }
                return;
            }
        };
        self.remember_readahead(
            start,
            buf,
            pending_submit.iter().map(|(slot, _)| slot.offset),
            &verdicts,
            entropy_epoch,
        );
        for (slot, entropy) in pending_submit {
            let at = slot.offset.0.saturating_sub(start.0) as usize;
            if matches!(
                verdicts.get(at),
                Some(super::chunk_crc::CrcVerdict::Mismatch)
            ) {
                // The span bytes were read before the verdict. Confirm under
                // `sync_in_progress`. Fresh bytes that match the stable row
                // are safe to pack. An unsettled offset is not.
                match self.note_if_crc_mismatch(slot.offset, ChunkType::Entropy, "entropy_sweep") {
                    Ok(super::CrcConfirm::NotConfirmed(fresh)) => {
                        self.submit_packed(slot, fresh, Some(read_done));
                    }
                    Ok(super::CrcConfirm::Confirmed) => {
                        self.fail_slot(
                            slot,
                            WriteDataChunkError::Other(eyre::eyre!("chunk crc mismatch")),
                        );
                    }
                    // Do not pack entropy that is still in `pending`. Ingress
                    // retries `Backpressure`. A mismatch error would fail the
                    // chunk for good.
                    Ok(super::CrcConfirm::Unsettled) => {
                        self.fail_slot(slot, WriteDataChunkError::Backpressure);
                    }
                    Err(error) => {
                        self.fail_slot(slot, WriteDataChunkError::Other(error));
                    }
                }
                continue;
            }
            if matches!(
                verdicts.get(at),
                Some(super::chunk_crc::CrcVerdict::Unknown)
            ) {
                super::chunk_crc::note_unknown("entropy_sweep");
            }
            self.submit_packed(slot, entropy, Some(read_done));
        }
    }

    /// One issued entropy pread. `slots` is the queued chunks it serves.
    /// `bytes` is the pread, including a hole the span reads through.
    fn note_issued_entropy_pread(&self, slots: usize, bytes: u64) {
        super::super::index_read_metrics::note_entropy_pread(slots as u64, bytes);
        #[cfg(any(test, feature = "bench"))]
        {
            self.disk.entropy_preads.fetch_add(1, Ordering::SeqCst);
            self.disk
                .entropy_pread_bytes
                .fetch_add(bytes, Ordering::SeqCst);
            self.disk
                .entropy_pread_slots
                .fetch_add(slots as u64, Ordering::SeqCst);
        }
    }

    /// Last chunk offset this pread covers. A short ingress span grows
    /// forward when no other chunk command is waiting.
    fn issued_read_end(&self, span: &[SweepSlot]) -> PartitionChunkOffset {
        let end = span[span.len() - 1].offset;
        let ahead = self.config.node_config.storage.entropy_readahead_bytes;
        if ahead == 0
            || span
                .iter()
                .any(|slot| slot.priority != WritePriority::Ingress)
        {
            return end;
        }
        // Other sweep slots are chunk work. Packed bytes still in the
        // pending map are a write the lane has not issued yet. Bytes
        // already pwritten (`written_ahead`) wait on the durable point
        // and do not take the disk.
        if !self.disk.queue().slots.is_empty() {
            return end;
        }
        {
            let pending = self.pending_writes.read().unwrap();
            if !pending.is_empty() {
                return end;
            }
        }
        let Some(tx_end) = self.tx_range_end(end) else {
            return end;
        };
        if tx_end <= end.0 {
            return end;
        }
        let chunk = self.config.consensus.chunk_size.max(1);
        let span_bytes = span_command_bytes(span[0].offset.0, end.0, chunk);
        let room = self.hold_cap_bytes().saturating_sub(span_bytes);
        let extra_chunks = ahead.min(room) / chunk;
        if extra_chunks == 0 {
            return end;
        }
        let extra = u32::try_from(extra_chunks).unwrap_or(u32::MAX);
        let mut last = end.0.saturating_add(extra).min(tx_end);
        if let Ok((interval, _)) = self.get_submodule_for_offset(end) {
            last = last.min(interval.end().0);
        }
        PartitionChunkOffset(last)
    }

    /// Last partition offset of the transaction that covers `offset`,
    /// clipped to this partition. `None` when the offset is not indexed.
    fn tx_range_end(&self, offset: PartitionChunkOffset) -> Option<u32> {
        let placement = self.data_root_and_tx_offset_span(offset, offset).ok()?;
        let (_root, tx_offset, data_size) = placement.into_iter().next().flatten()?;
        let chunk = self.config.consensus.chunk_size.max(1);
        let chunks = data_size.div_ceil(chunk);
        if chunks == 0 {
            return None;
        }
        let last_tx = u32::try_from(chunks - 1).ok()?;
        let start = offset.0.checked_sub(u32::from(tx_offset))?;
        let tx_last = start.checked_add(last_tx)?;
        let partition_last = u32::try_from(
            self.config
                .consensus
                .num_chunks_in_partition
                .saturating_sub(1),
        )
        .unwrap_or(u32::MAX);
        Some(tx_last.min(partition_last))
    }

    /// Keep entropy the pread covered and this span did not pack.
    ///
    /// Lock order is the interval map, then `pending_writes`, then the
    /// cache. A write takes `pending_writes` and then the cache, so it
    /// cannot land between the check and the insert.
    fn remember_readahead(
        &self,
        start: PartitionChunkOffset,
        buf: &[u8],
        packed: impl Iterator<Item = PartitionChunkOffset>,
        verdicts: &[super::chunk_crc::CrcVerdict],
        entropy_epoch: u64,
    ) {
        let cap = self
            .config
            .node_config
            .storage
            .entropy_readahead_cache_bytes;
        if cap == 0 {
            return;
        }
        let chunk_len = self.config.consensus.chunk_size.max(1) as usize;
        if chunk_len == 0 || !buf.len().is_multiple_of(chunk_len) {
            return;
        }
        let packed: std::collections::HashSet<u32> = packed.map(|offset| offset.0).collect();
        let generation = self.index_write_generation.load(Ordering::SeqCst);
        let intervals = self.intervals.read().unwrap();
        // The bump is taken under this same lock. A mismatch means the buffer
        // was read before a newer entropy pwrite.
        if self.entropy_write_epoch.load(Ordering::SeqCst) != entropy_epoch {
            return;
        }
        let pending = self.pending_writes.read().unwrap();
        if self.index_write_generation.load(Ordering::SeqCst) != generation {
            return;
        }
        let mut cache = self
            .entropy_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let count = buf.len() / chunk_len;
        for step in 0..count {
            let off = start.0.saturating_add(step as u32);
            if packed.contains(&off) {
                continue;
            }
            let offset = PartitionChunkOffset(off);
            if pending.contains_key(&offset)
                || pending.written_ahead.contains_key(&offset)
                || pending.occupancy.contains_key(&offset)
            {
                continue;
            }
            if !intervals
                .get_at_point(offset)
                .is_some_and(|kind| *kind == ChunkType::Entropy)
            {
                continue;
            }
            if matches!(
                verdicts.get(step),
                Some(super::chunk_crc::CrcVerdict::Mismatch)
            ) {
                continue;
            }
            let at = step * chunk_len;
            if cache.insert(
                offset,
                buf[at..at + chunk_len].to_vec(),
                generation,
                cap,
                entropy_epoch,
            ) {
                super::super::index_read_metrics::note_readahead_store();
                #[cfg(any(test, feature = "bench"))]
                self.disk
                    .entropy_readahead_stores
                    .fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    fn restore_span(&self, span: Vec<SweepSlot>) {
        let displaced = {
            let mut queue = self.disk.queue();
            let mut displaced = Vec::new();
            for slot in span {
                if let Some(previous) = queue.insert_slot(slot) {
                    displaced.push(previous);
                }
            }
            displaced
        };
        self.retire_displaced_slots(displaced);
    }

    /// Finish a slot `insert_slot` removed. The group gets backpressure and
    /// its unpacked length leaves `queued_unpacked_bytes`.
    fn retire_displaced_slots(&self, displaced: Vec<SweepSlot>) {
        for slot in displaced {
            self.release_queued_offset(slot.offset, slot.generation, slot.byte_len);
            self.mark_group_slot_done(slot.group, Some(WriteDataChunkError::Backpressure));
        }
    }

    fn fail_span(&self, span: Vec<SweepSlot>, error: &WriteDataChunkError) {
        for slot in span {
            let group = slot.group;
            self.fail_slot(slot, clone_error(error));
            self.fail_queued_group(group);
        }
    }

    #[cfg(test)]
    fn take_disk_span(&self) -> Option<Vec<SweepSlot>> {
        self.take_ready_span()
    }

    /// Lowest-offset span that is ready to read.
    ///
    /// The walk stops there. Later spans stay queued and are not rebuilt, so
    /// one take does not rescan the backlog.
    fn take_ready_span(&self) -> Option<Vec<SweepSlot>> {
        let mut queue = self.disk.queue();
        let offsets = self.first_ready_span(&queue)?;
        let mut span = Vec::with_capacity(offsets.len());
        for offset in offsets {
            let Some(slot) = queue.remove_slot(offset) else {
                continue;
            };
            span.push(slot);
        }
        (!span.is_empty()).then_some(span)
    }

    fn has_ready_disk_span(&self) -> bool {
        let queue = self.disk.queue();
        if queue.disk_len() == 0 {
            return false;
        }
        self.first_ready_span(&queue).is_some()
    }

    /// Offsets of the lowest-offset ready disk span. `None` when every disk
    /// span is still inside the read grace, or is waiting on a queued write
    /// at one of its offsets. A reread stays queued until that write lands.
    /// Counting it as ready would spin the lane: the sweep puts it back and
    /// the next pass takes it again.
    fn first_ready_span(&self, queue: &SweepQueue) -> Option<Vec<PartitionChunkOffset>> {
        let chunk = self.config.consensus.chunk_size.max(1);
        // `queue` is already held. `pending_writes` is the later lock.
        let pending = self.pending_writes.read().unwrap();
        let mut found = None;
        self.for_each_disk_span(&queue.slots, |span| {
            if disk_span_waits_on_write(&pending, span) {
                return false;
            }
            if self.span_slots_ready(span, chunk) {
                found = Some(span.iter().map(|slot| slot.offset).collect());
                return true;
            }
            false
        });
        found
    }

    /// Milliseconds configured for a short entropy span. `0` does not wait.
    fn entropy_read_grace(&self) -> Duration {
        Duration::from_millis(self.config.node_config.storage.entropy_read_grace_ms)
    }

    /// Bytes that skip the read grace. `0` means every span is large enough.
    /// The sweep cap still limits the pread, so the target cannot exceed it.
    fn entropy_batch_target(&self) -> u64 {
        let configured = self.config.node_config.storage.entropy_read_batch_bytes;
        if configured == 0 {
            return 0;
        }
        configured.min(self.hold_cap_bytes())
    }

    /// A span may be issued. Grace `0`, a packing slot, a span at the batch
    /// target, or an oldest slot that has already waited.
    fn span_slots_ready(&self, span: &[QueuedOffset], chunk: u64) -> bool {
        let grace = self.entropy_read_grace();
        if grace.is_zero() {
            return true;
        }
        if span
            .iter()
            .any(|slot| slot.priority == WritePriority::Packing)
        {
            return true;
        }
        let target = self.entropy_batch_target();
        if target == 0 || span_queued_bytes(span, chunk) >= target {
            return true;
        }
        let Some(oldest) = span.iter().map(|slot| slot.enqueued_at).min() else {
            return false;
        };
        oldest.elapsed() >= grace
    }

    /// Submodule that owns `offset`, or a key unique to this offset when no
    /// submodule does. Unassigned offsets do not coalesce with each other.
    fn disk_span_group(&self, offset: PartitionChunkOffset) -> SpanGroup {
        match self.submodules.get_key_value_at_point(offset) {
            Ok((interval, _)) => SpanGroup::Submodule(interval.start().0),
            Err(_) => SpanGroup::Lone(offset.0),
        }
    }

    /// Disk spans in offset order. The walk stops when `visit` returns true.
    ///
    /// Memory slots are skipped and do not split a disk span. The gap across
    /// them still counts. Slots are already in offset order, and submodule
    /// intervals do not overlap, so closing a span when the group changes or
    /// the coalesce cap is hit matches grouping then sorting by start.
    fn for_each_disk_span(
        &self,
        slots: &BTreeMap<PartitionChunkOffset, SweepSlot>,
        mut visit: impl FnMut(&[QueuedOffset]) -> bool,
    ) {
        let chunk = self.config.consensus.chunk_size.max(1);
        let hole = self.config.node_config.storage.entropy_coalesce_hole_bytes;
        let cap = self.hold_cap_bytes();
        let mut open: Vec<QueuedOffset> = Vec::new();
        let mut group = None;
        for (offset, slot) in slots {
            if memory_entropy(slot) {
                continue;
            }
            #[cfg(test)]
            self.disk.span_walk_slots.fetch_add(1, Ordering::Relaxed);
            let next_group = self.disk_span_group(*offset);
            let next = QueuedOffset {
                offset: *offset,
                priority: slot.priority,
                enqueued_at: slot.enqueued_at,
            };
            if let Some(current) = group {
                let start = open[0].offset.0;
                let end = open[open.len() - 1].offset.0;
                let grows = current == next_group
                    && span_can_grow(start, end, next.offset.0, chunk, hole, cap);
                if !grows {
                    if visit(&open) {
                        return;
                    }
                    open.clear();
                    group = None;
                }
            }
            if open.is_empty() {
                group = Some(next_group);
            }
            open.push(next);
        }
        if !open.is_empty() {
            let _ = visit(&open);
        }
    }

    fn pack_one(&self, slot: SweepSlot) {
        let group = slot.group;
        if let Err(error) = self.slot_still_valid(&slot, None) {
            if matches!(error, WriteDataChunkError::Backpressure) {
                let _ = self.reject_backpressure(slot, true);
            } else {
                self.fail_slot(slot, error);
                self.fail_queued_group(group);
            }
            return;
        }
        let Some(entropy) = slot
            .pending_entropy
            .clone()
            .or_else(|| slot.cached_entropy.clone())
        else {
            self.fail_slot(
                slot,
                WriteDataChunkError::Other(eyre::eyre!("missing buffered entropy")),
            );
            self.fail_queued_group(group);
            return;
        };
        self.submit_packed(slot, entropy, None);
    }

    fn submit_packed(&self, slot: SweepSlot, entropy: Vec<u8>, read_done: Option<Instant>) {
        let group = slot.group;
        let priority = slot.priority;
        let enqueued_at = slot.enqueued_at;
        let group_failed = self
            .disk
            .queue()
            .groups
            .get(&group)
            .is_some_and(|entry| entry.failed.is_some());
        if group_failed {
            self.fail_slot(slot, WriteDataChunkError::WritesPaused);
            return;
        }
        // Recheck immediately before the chunk is queued. A pause or reset
        // that landed after the read must not publish this offset.
        if let Err(error) = self.slot_still_valid(&slot, Some(&entropy)) {
            if matches!(error, WriteDataChunkError::Backpressure) {
                let _ = self.reject_backpressure(slot, true);
            } else {
                self.fail_slot(slot, error);
                self.fail_queued_group(group);
            }
            return;
        }
        // Off the queue. A newer entropy write can land during the XOR.
        #[cfg(any(test, feature = "test-utils"))]
        self.disk
            .entropy_pack_pause
            .wait_if(EntropyPackPause::BeforePackXor);
        // The data-path row is written after `chunks.dat` is fdatasync'd.
        // Failing here leaves no pending bytes and no index row.
        #[cfg(any(test, feature = "test-utils"))]
        if self.take_index_commit_failure() {
            self.fail_slot(
                slot,
                WriteDataChunkError::Other(eyre::eyre!("injected index commit failure")),
            );
            self.fail_queued_group(group);
            return;
        }
        let offset = slot.offset;
        let copied_pad = slot.pending_entropy.is_none();
        if self.submodules.get_key_value_at_point(offset).is_err() {
            self.fail_slot(
                slot,
                WriteDataChunkError::Other(eyre::eyre!(
                    "No submodule found for Partition Offset {offset:?}"
                )),
            );
            self.fail_queued_group(group);
            return;
        }
        let pack_started = Instant::now();
        let cache_hit = slot.cached_entropy.is_some();
        let packed = match packing_xor_vec_u8(entropy, &slot.unpacked) {
            Ok(packed) => packed,
            Err(error) => {
                self.fail_slot(slot, WriteDataChunkError::Other(error));
                self.fail_queued_group(group);
                return;
            }
        };
        let mut disk_compare = Duration::ZERO;
        let inserted = self.commit_packed(
            slot.offset,
            packed,
            slot.generation,
            slot.priority,
            slot.byte_len,
            (slot.path_hash, (*slot.data_path).clone()),
            copied_pad,
            slot.entropy_epoch,
            slot.pending_entropy.as_deref(),
            &slot.unpacked,
            &mut disk_compare,
        );
        if inserted.is_ok() && cache_hit {
            super::super::index_read_metrics::note_readahead_hit();
            #[cfg(any(test, feature = "bench"))]
            self.disk
                .entropy_readahead_hits
                .fetch_add(1, Ordering::SeqCst);
        }
        if inserted.is_ok() && priority == WritePriority::Ingress {
            // The pread already returned. Record that interval only when the
            // body is queued. A rejected read is swept again and would
            // record the same stage twice.
            if let Some(read_done) = read_done {
                self.note_ingress_stage(
                    "queue_to_entropy",
                    read_done.saturating_duration_since(enqueued_at),
                );
            }
            // The copied-pad compare preads under the chunk-file lock. That
            // wait is not the XOR or the packed insert.
            self.note_ingress_stage(
                "entropy_to_packed",
                pack_started.elapsed().saturating_sub(disk_compare),
            );
        }
        if let Err(error) = inserted {
            if matches!(error, WriteDataChunkError::Backpressure) {
                let _ = self.reject_backpressure(slot, false);
                return;
            }
            self.mark_group_slot_done(group, Some(error));
            return;
        }
        self.mark_group_slot_done(group, None);
    }

    fn note_ingress_stage(&self, stage: &'static str, elapsed: Duration) {
        super::super::index_read_metrics::note_ingest_stage(stage, elapsed);
        #[cfg(test)]
        self.disk
            .ingest_stages
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((stage, elapsed));
    }

    fn slot_still_valid(
        &self,
        slot: &SweepSlot,
        pread: Option<&[u8]>,
    ) -> Result<(), WriteDataChunkError> {
        if self.data_writes_paused()
            || self.index_write_generation.load(Ordering::SeqCst) != slot.generation
        {
            return Err(WriteDataChunkError::WritesPaused);
        }
        {
            let pending = self.pending_writes.read().unwrap();
            if pending.occupancy.get(&slot.offset).copied() != Some(slot.generation) {
                return Err(WriteDataChunkError::WritesPaused);
            }
            // The pad in the slot was copied earlier. A write that now owns
            // this offset has newer entropy. XOR would publish the old pad.
            if slot.cached_entropy.is_some()
                && slot.pending_entropy.is_none()
                && (pending.contains_key(&slot.offset)
                    || pending.written_ahead.contains_key(&slot.offset))
            {
                return Err(WriteDataChunkError::Backpressure);
            }
            // The clone is the pack target only while the map still holds
            // those bytes. A newer enqueue does not bump the epoch, and a
            // durable point removes the entry. The interval may still be the
            // type from before this queued write.
            if let Some(cloned) = slot.pending_entropy.as_deref() {
                let same = pending.get(&slot.offset).is_some_and(|(bytes, ty)| {
                    *ty == ChunkType::Entropy && bytes.as_slice() == cloned
                });
                if same {
                    return Ok(());
                }
                return Err(WriteDataChunkError::Backpressure);
            }
        }
        // The bytes were copied earlier: a disk pread or a cache pad. An
        // entropy pwrite bumps the module epoch for every offset, then
        // clears `pending` and `written_ahead` only for the offsets it
        // wrote. A pad at another offset is still the file bytes. Compare
        // this offset when the epoch moved. A mismatch is a rewrite here.
        let copied = pread.or(slot.cached_entropy.as_deref());
        let current = match copied {
            Some(bytes) => self.copied_bytes_current(slot.offset, bytes, slot.entropy_epoch)?,
            None => self.entropy_write_epoch.load(Ordering::SeqCst) == slot.entropy_epoch,
        };
        if !current {
            return Err(WriteDataChunkError::Backpressure);
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
            let offsets: Vec<PartitionChunkOffset> = queue
                .slots
                .iter()
                .filter(|(_, slot)| pred(slot))
                .map(|(offset, _)| *offset)
                .collect();
            let mut removed = Vec::with_capacity(offsets.len());
            for offset in offsets {
                if let Some(slot) = queue.remove_slot(offset) {
                    removed.push(slot);
                }
            }
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

    fn commit_packed(
        &self,
        offset: PartitionChunkOffset,
        packed: Vec<u8>,
        generation: u64,
        priority: WritePriority,
        unpacked_len: u64,
        proof: (ChunkPathHash, ChunkDataPath),
        copied_pad: bool,
        entropy_epoch: u64,
        pending_entropy: Option<&[u8]>,
        unpacked: &[u8],
        disk_compare: &mut Duration,
    ) -> Result<(), WriteDataChunkError> {
        // The compare preads. It must not run under `pending_writes`: a
        // pwrite takes the file lock and then that map.
        for _ in 0..8 {
            let live = self.entropy_write_epoch.load(Ordering::SeqCst);
            let confirmed = if copied_pad && live != entropy_epoch {
                let compare_started = Instant::now();
                let entropy = packing_xor_vec_u8(packed.clone(), unpacked)?;
                let matched = self.stable_disk_matches(offset, &entropy);
                *disk_compare = disk_compare.saturating_add(compare_started.elapsed());
                match matched {
                    Ok(true) => self.entropy_write_epoch.load(Ordering::SeqCst),
                    Ok(false) => {
                        return self.abandon_copied_commit(offset, generation, unpacked_len);
                    }
                    Err(error) => {
                        let _ = self.abandon_copied_commit(offset, generation, unpacked_len);
                        return Err(error);
                    }
                }
            } else {
                live
            };
            let mut pending = self.pending_writes.write().unwrap();
            pending.queued_unpacked_bytes =
                pending.queued_unpacked_bytes.saturating_sub(unpacked_len);
            if pending.occupancy.get(&offset).copied() != Some(generation) {
                return Err(WriteDataChunkError::WritesPaused);
            }
            // `written_ahead` is a pwrite this commit did not do. Leave it.
            // A copied pad must not replace entropy that landed after the
            // copy. A module epoch bump from another offset leaves this
            // pad's bytes on disk; `confirmed` is that check. A pending
            // clone is packed only while the map still holds those bytes:
            // the XOR does not hold `pending_writes`, and a newer enqueue
            // does not bump the epoch.
            let pending_clone_stale = match pending_entropy {
                Some(cloned) => !pending.get(&offset).is_some_and(|(bytes, ty)| {
                    *ty == ChunkType::Entropy && bytes.as_slice() == cloned
                }),
                None => false,
            };
            if pending.written_ahead.contains_key(&offset)
                || (copied_pad && pending.contains_key(&offset))
                || pending_clone_stale
            {
                pending.occupancy.remove(&offset);
                return Err(WriteDataChunkError::Backpressure);
            }
            if copied_pad && self.entropy_write_epoch.load(Ordering::SeqCst) != confirmed {
                pending.queued_unpacked_bytes =
                    pending.queued_unpacked_bytes.saturating_add(unpacked_len);
                drop(pending);
                continue;
            }
            pending.occupancy.remove(&offset);
            if self.data_writes_paused()
                || self.index_write_generation.load(Ordering::SeqCst) != generation
            {
                return Err(WriteDataChunkError::WritesPaused);
            }
            pending.insert(offset, (packed, ChunkType::Data));
            pending.priorities.insert(offset, priority);
            self.entropy_cache
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .invalidate(offset);
            pending.queued_at.insert(offset, Instant::now());
            pending.proofs.insert(offset, proof);
            *self.last_pending_write.write().unwrap() = Instant::now();
            drop(pending);
            self.disk.notify();
            return Ok(());
        }
        self.abandon_copied_commit(offset, generation, unpacked_len)
    }

    /// Release the reserved offset after a copied pad failed the disk check.
    fn abandon_copied_commit(
        &self,
        offset: PartitionChunkOffset,
        generation: u64,
        unpacked_len: u64,
    ) -> Result<(), WriteDataChunkError> {
        let mut pending = self.pending_writes.write().unwrap();
        pending.queued_unpacked_bytes = pending.queued_unpacked_bytes.saturating_sub(unpacked_len);
        if pending.occupancy.get(&offset).copied() != Some(generation) {
            return Err(WriteDataChunkError::WritesPaused);
        }
        pending.occupancy.remove(&offset);
        Err(WriteDataChunkError::Backpressure)
    }

    /// `bytes` were copied at `stored_epoch`. True when they are still the
    /// entropy at `offset`. An epoch bump from a different offset keeps
    /// these bytes. A rewrite of `offset` changes them.
    fn copied_bytes_current(
        &self,
        offset: PartitionChunkOffset,
        bytes: &[u8],
        stored_epoch: u64,
    ) -> Result<bool, WriteDataChunkError> {
        if self.entropy_write_epoch.load(Ordering::SeqCst) == stored_epoch {
            return Ok(true);
        }
        self.stable_disk_matches(offset, bytes)
    }

    /// True when a pread of `offset` equals `bytes` and the epoch does not
    /// move across that pread. A move can be this offset or another, so the
    /// pread is repeated. A mismatch is a rewrite of this offset.
    fn stable_disk_matches(
        &self,
        offset: PartitionChunkOffset,
        bytes: &[u8],
    ) -> Result<bool, WriteDataChunkError> {
        for _ in 0..8 {
            let before = self.entropy_write_epoch.load(Ordering::SeqCst);
            let disk = self
                .pread_entropy_chunk(offset)
                .map_err(WriteDataChunkError::Other)?;
            let after = self.entropy_write_epoch.load(Ordering::SeqCst);
            if disk.as_slice() != bytes {
                return Ok(false);
            }
            if before == after {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn pread_entropy_chunk(&self, offset: PartitionChunkOffset) -> eyre::Result<Vec<u8>> {
        let chunk_len = self.config.consensus.chunk_size.max(1) as usize;
        let (file_arc, file_offset, _) = self.locate_io(offset, true)?;
        let mut buf = vec![0_u8; chunk_len];
        let file = self.disk.lock_chunks(&file_arc);
        file.read_exact_at(&mut buf, file_offset)
            .map_err(|error| eyre::eyre!("entropy pad check at offset {offset}: {error}"))?;
        Ok(buf)
    }

    /// Queue `slot` for a disk read when nobody is waiting on its group.
    /// `true` when the slot was queued. Otherwise the caller still owns the
    /// failure: a waiter gets backpressure, and the batch path has no waiter
    /// only when the offset was taken by another body.
    fn reject_backpressure(&self, slot: SweepSlot, occupancy_held: bool) -> bool {
        let group = slot.group;
        match self.try_reread(slot, occupancy_held) {
            Ok(()) => true,
            Err(slot) => {
                if occupancy_held {
                    self.fail_slot(slot, WriteDataChunkError::Backpressure);
                    self.fail_queued_group(group);
                } else {
                    self.mark_group_slot_done(group, Some(WriteDataChunkError::Backpressure));
                }
                false
            }
        }
    }

    fn try_reread(&self, mut slot: SweepSlot, occupancy_held: bool) -> Result<(), SweepSlot> {
        if !self.group_is_unwaited(slot.group) {
            return Err(slot);
        }
        if !occupancy_held && !self.reclaim_occupancy(slot.offset, slot.generation, slot.byte_len) {
            return Err(slot);
        }
        slot.cached_entropy = None;
        slot.pending_entropy = None;
        slot.entropy_epoch = 0;
        self.restore_span(vec![slot]);
        Ok(())
    }

    fn group_is_unwaited(&self, group: u64) -> bool {
        let queue = self.disk.queue();
        queue
            .groups
            .get(&group)
            .is_some_and(|entry| !entry.waited && entry.notify.is_none())
    }

    fn reclaim_occupancy(
        &self,
        offset: PartitionChunkOffset,
        generation: u64,
        byte_len: u64,
    ) -> bool {
        let mut pending = self.pending_writes.write().unwrap();
        if pending.occupancy.contains_key(&offset) {
            return false;
        }
        pending.occupancy.insert(offset, generation);
        pending.queued_unpacked_bytes = pending.queued_unpacked_bytes.saturating_add(byte_len);
        true
    }

    /// A disk slot whose offset still has a queued write. Reading it now
    /// would pack the bytes from before that write.
    fn span_blocked_on_pending(&self, span: &[SweepSlot]) -> bool {
        let pending = self.pending_writes.read().unwrap();
        span.iter().any(|slot| {
            !memory_entropy(slot)
                && (pending.contains_key(&slot.offset)
                    || pending.written_ahead.contains_key(&slot.offset))
        })
    }

    /// True when every disk slot is waiting on `pending` or `written_ahead`
    /// and no memory slot can fold. A lower-priority memory slot stays
    /// queued: this pass cannot pack it ahead of the blocked reread, and
    /// the sweep wait does not commit the write that blocks the pread.
    /// A memory slot at the best priority can still fold. A queue with no
    /// disk slot is not blocked. A neighbor outside those maps can still
    /// join a parked offset into one span. The drain uses the span walk
    /// for that case.
    #[cfg(test)]
    fn only_pending_blocked_disk_slots(&self) -> bool {
        let disk_offsets = {
            let queue = self.disk.queue();
            if queue.slots.is_empty() {
                return false;
            }
            if queue
                .best_priority()
                .is_some_and(|best| queue.memory_slots[priority_index(best)] > 0)
            {
                return false;
            }
            let mut disk_offsets = Vec::new();
            for slot in queue.slots.values() {
                if memory_entropy(slot) {
                    continue;
                }
                disk_offsets.push(slot.offset);
            }
            if disk_offsets.is_empty() {
                return false;
            }
            disk_offsets
        };
        let pending = self.pending_writes.read().unwrap();
        disk_offsets.into_iter().all(|offset| {
            pending.contains_key(&offset) || pending.written_ahead.contains_key(&offset)
        })
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

    /// Entropy preads issued on this module: count, bytes, queued slots.
    #[cfg(any(test, feature = "bench"))]
    pub fn bench_entropy_reads(&self) -> (u64, u64, u64, u64, u64) {
        (
            self.disk.entropy_preads.load(Ordering::Relaxed),
            self.disk.entropy_pread_bytes.load(Ordering::Relaxed),
            self.disk.entropy_pread_slots.load(Ordering::Relaxed),
            self.disk.entropy_readahead_hits.load(Ordering::Relaxed),
            self.disk.entropy_readahead_stores.load(Ordering::Relaxed),
        )
    }

    #[cfg(test)]
    pub(super) fn entropy_cache_offsets_for_test(&self) -> Vec<u32> {
        let cache = self
            .entropy_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut offsets: Vec<u32> = cache.pads.keys().map(|offset| offset.0).collect();
        offsets.sort_unstable();
        offsets
    }

    #[cfg(test)]
    pub(super) fn sweep_slot_count_for_test(&self) -> usize {
        self.disk.queue().slots.len()
    }

    /// Lane path. Up to `INFLIGHT_WRITES` pwrites share the module file.
    /// A recall that holds the disk stops new submits. Calls already
    /// submitted keep running.
    pub(super) fn write_runs_window(&self, runs: &[WriteRun]) -> eyre::Result<()> {
        self.disk.wait_until_idle();
        let mut next = 0;
        while next < runs.len() {
            let mut guards = Vec::with_capacity(INFLIGHT_WRITES);
            while guards.len() < INFLIGHT_WRITES && next + guards.len() < runs.len() {
                let bytes = runs[next + guards.len()].bytes.len() as u64;
                if self.disk.recall_holds() || !self.disk.try_submit_write(INFLIGHT_WRITES, bytes) {
                    break;
                }
                guards.push(OpGuard {
                    disk: &self.disk,
                    bytes,
                });
            }
            if guards.is_empty() {
                if self.disk.recall_holds() {
                    self.disk.yield_to_recall();
                } else {
                    let bytes = runs[next].bytes.len() as u64;
                    self.disk.wait_for_write_slot(bytes);
                }
                continue;
            }
            let batch = &runs[next..next + guards.len()];
            let reserved = guards.len();
            // A worker panic must return here. `commit_pending_runs` still
            // holds `sync_in_progress`. Unwinding that guard poisons it, and
            // later flushes and `reset` cannot take the lock.
            let results = std::thread::scope(|scope| {
                let mut handles = Vec::with_capacity(reserved);
                for (run, guard) in batch.iter().zip(guards) {
                    handles.push(scope.spawn(move || {
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            self.pwrite_run_inflight(run, guard)
                        }))
                    }));
                }
                handles
                    .into_iter()
                    .map(|handle| match handle.join() {
                        Ok(Ok(result)) => result,
                        Ok(Err(_)) | Err(_) => Err(eyre::eyre!("chunk write panicked")),
                    })
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
        #[cfg(test)]
        if self.pwrite_panic_once.swap(false, Ordering::SeqCst) {
            panic!("injected chunk write panic");
        }
        // A test failure takes the same path as `EIO`: the guard drops first,
        // then the live label returns to `Uninitialized` and the bytes stay queued.
        #[cfg(test)]
        if self.disk.take_pwrite_failure() {
            drop(guard);
            let error = std::io::Error::other("injected pwrite failure");
            return self.note_write_run(
                run,
                Instant::now(),
                Err(Self::chunk_write_report(run.start, 0_u64, &error)),
            );
        }
        let (file_arc, file_offset, submodule_offset) = self.locate_io(run.start, true)?;
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
    /// poll. A wake bumps that epoch under the same lock, so a wake that
    /// already ran is polled before the next sleep.
    fn wait_until_lane_event(&self, mut seen: u64) -> u64 {
        loop {
            if self.disk.stop_requested() || self.lane_work_ready() {
                return self.disk.wake.epoch();
            }
            // A zero delay means a command is already due. If the disk is
            // still busy, `lane_work_ready` is false and the next notify
            // (a finished command) is the wake. A positive delay is the
            // rest of the write grace or the read grace, whichever is sooner.
            // A retry instant can pass between the ready check and this
            // grace. The zero is dropped below, so check readiness again
            // before sleeping with no timeout.
            let timeout = self.lane_grace_remaining().filter(|delay| !delay.is_zero());
            if timeout.is_none() && self.lane_work_ready() {
                return self.disk.wake.epoch();
            }
            // Sample before the wake lock. A lane command already in the
            // kernel, or a syscall holding the file, is service time.
            let idle_reason = self.lane_sleep_reason();
            let started = Instant::now();
            let mut guard = self.disk.lock_wake();
            if self.disk.stop_requested() {
                return self.disk.wake.epoch();
            }
            let epoch = self.disk.wake.epoch();
            if epoch != seen {
                drop(guard);
                seen = epoch;
                continue;
            }
            guard = match timeout {
                Some(delay) => self.disk.wake.wait_timeout(guard, delay),
                None => self.disk.wake.wait(guard),
            };
            drop(guard);
            if let Some(reason) = idle_reason {
                super::super::index_read_metrics::note_disk_idle(reason, started.elapsed());
            }
        }
    }

    /// Why the lane is about to sleep with no chunk command in the kernel.
    /// `None` when that sleep is service time: a lane `pread`/`pwrite` is
    /// out, or a syscall holds `chunks.dat`.
    fn lane_sleep_reason(&self) -> Option<&'static str> {
        if self.disk.disk_ops.load(Ordering::SeqCst) != 0
            || self.disk.io_exclusive.load(Ordering::SeqCst) != 0
        {
            return None;
        }
        Some(self.disk_idle_reason())
    }

    /// Why no entropy read is being issued. A mining recall's own `pread`
    /// is not a lane command, so `recall_hold` includes that service time.
    /// `irys.storage.recall_pread_us_total` is the pread itself.
    fn disk_idle_reason(&self) -> &'static str {
        if self.disk.recall_holds() {
            return "recall_hold";
        }
        if self.disk.writes_active() {
            return "packed_write_hold";
        }
        if self.disk.io_exclusive.load(Ordering::SeqCst) > 0 {
            return "exclusive_io";
        }
        match self.sync_in_progress.try_lock() {
            Ok(guard) => drop(guard),
            Err(TryLockError::WouldBlock) => return "sync_in_progress",
            Err(TryLockError::Poisoned(poisoned)) => {
                drop(poisoned.into_inner());
            }
        }
        if !self.disk.disk_available() {
            return "inflight_budget";
        }
        // A queued disk slot that is waiting on a write is not a read.
        // `None` means no span is inside the read grace. Parked bytes wait
        // on the durable point, and that sleep is not read grace.
        if self.read_grace_remaining().is_some() {
            return "read_grace";
        }
        if self.durable_point_remaining().is_some() {
            return "durable_point";
        }
        "nothing_queued"
    }

    /// Time until the next packed write is allowed.
    /// `None` when nothing is queued. Zero when the disk is idle, a full run
    /// is ready, the durability count is reached, or an aged short run is
    /// ready. A young short run waits out `reorder_grace` while the disk
    /// stays busy. A failed write returns the time left on its retry instant
    /// instead of zero: the lane wait drops a zero timeout and would then
    /// sleep until some other notify.
    fn write_grace_remaining(&self) -> Option<Duration> {
        let pending = self.pending_writes.read().unwrap();
        let oldest = pending.queued_at.values().copied().min()?;
        if let Some(remain) = self.disk.write_retry_remaining() {
            return Some(remain);
        }
        if self.disk.chunk_disk_idle() {
            return Some(Duration::ZERO);
        }
        let chunk = self.config.consensus.chunk_size.max(1);
        let grace = reorder_grace(chunk);
        if self.short_runs_due(pending.len())
            || self.has_full_write_run(&pending, WRITE_RUN_MAX_BYTES.max(chunk))
            || oldest.elapsed() >= grace
        {
            return Some(Duration::ZERO);
        }
        Some(grace.saturating_sub(oldest.elapsed()))
    }

    /// Time until the next entropy read is allowed.
    /// `None` when no disk slot is queued. Zero when the grace is off, a span
    /// is already eligible, or the oldest chunk in some span has waited out
    /// `entropy_read_grace_ms`. Otherwise the time left on the soonest span.
    /// The wait runs while the disk is idle. Recall, a packed-write hold, and
    /// the in-flight budget still gate the read.
    fn read_grace_remaining(&self) -> Option<Duration> {
        let queue = self.disk.queue();
        if queue.disk_len() == 0 {
            return None;
        }
        let grace = self.entropy_read_grace();
        let chunk = self.config.consensus.chunk_size.max(1);
        // Same lock order as `first_ready_span`: the queue, then this map.
        // A span waiting on a queued write is not due. A zero grace would
        // otherwise hide the durable-point timer and busy-loop the lane.
        let pending = self.pending_writes.read().unwrap();
        let mut seen = false;
        let mut ready = false;
        let mut soonest = grace;
        self.for_each_disk_span(&queue.slots, |span| {
            if disk_span_waits_on_write(&pending, span) {
                return false;
            }
            seen = true;
            if self.span_slots_ready(span, chunk) {
                ready = true;
                return true;
            }
            if let Some(oldest) = span.iter().map(|slot| slot.enqueued_at).min() {
                soonest = soonest.min(grace.saturating_sub(oldest.elapsed()));
            }
            false
        });
        if !seen {
            return None;
        }
        if ready {
            Some(Duration::ZERO)
        } else {
            Some(soonest)
        }
    }

    /// Soonest of the write grace, the read grace, and a due interval
    /// completion. `None` only when nothing is waiting on a timer.
    fn lane_grace_remaining(&self) -> Option<Duration> {
        [
            self.write_grace_remaining(),
            self.read_grace_remaining(),
            self.interval_flush_remaining(),
            self.durable_point_remaining(),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// A pass can run. A recall, a file holder, or a full window is not ready.
    /// A short entropy span waits out its grace even when the disk is idle.
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
        // A due interval completion rewrites even when no chunk is queued.
        // Zero remaining must count as ready: `wait_until_lane_event` treats
        // a zero grace as "wait for a notify".
        if self.interval_flush_remaining() == Some(Duration::ZERO) {
            return true;
        }
        // A deferred durable point becomes ready when its interval elapses,
        // when pending bytes cross the headroom line, or when they are over
        // the ceiling. Zero remaining must count as ready: a zero grace
        // waits for a notify.
        if self.durable_point_remaining() == Some(Duration::ZERO) {
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
        let Some(best) = queue.best_priority() else {
            return false;
        };
        queue.memory_slots[priority_index(best)] > 0
    }

    fn disk_lane_pass(&self) {
        // A recall that holds the disk sets `recall_waiters` before it reads.
        // Submit nothing until that hold drops.
        if self.disk.recall_holds() {
            return;
        }
        // Completion rewrites wait out an active recall, so a recall that is
        // already waiting is not blocked on the interval file.
        if let Err(error) = self.flush_interval_completions_locked(false) {
            tracing::error!(
                "Couldn't rewrite intervals for storage_module {}: {error}",
                self.id
            );
        }
        // A recall can arrive during the interval rewrite. It owns the disk
        // before this pass writes or reads entropy.
        if self.disk.recall_holds() {
            return;
        }
        // An external flush or interval rewrite runs here, before this pass
        // chooses a chunk command. A recall above skips it until the `pread`
        // returns, so the mining read is not cut by the file update.
        self.service_lane_handoff();
        // Capture this before the flush clears it. Entropy stays behind an
        // owed recall flush so the waiting recall gets the disk next.
        // Packed writes go before entropy reads. A sweep in this pass must
        // not start after the write window if a recall arrived during it.
        let owed_flush = self.disk.recall_flush_is_owed();
        if owed_flush {
            if let Err(error) = self.flush_packed_window() {
                tracing::error!(
                    "Couldn't flush packed chunks after recall for storage_module {}: {error}",
                    self.id
                );
            }
        } else if self.pending_run_ready() {
            self.write_ready_runs();
        }
        // After the owed flag drops, so a recall does not wait on a pair
        // this pass deferred. A due pair runs here.
        if self.durable_point_remaining() == Some(Duration::ZERO)
            && let Err(error) = self.finish_deferred_durable_point()
        {
            tracing::error!(
                "Couldn't finish a deferred durable point for storage_module {}: {error}",
                self.id
            );
        }
        if !owed_flush && !self.disk.recall_holds() {
            self.pump_entropy_reads();
        }
    }

    fn write_ready_runs(&self) {
        match self.commit_pending_runs(false, Some(INFLIGHT_WRITES), true) {
            Ok(()) => self.disk.clear_write_retry(),
            Err(error) => {
                tracing::error!(
                    "Couldn't write packed chunks for storage_module {}: {error}",
                    self.id
                );
                // The bytes stay queued. The next pass waits out the retry
                // instant. Shutdown and `force_sync` do not.
                self.disk.arm_write_retry();
            }
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
            if request.force {
                match self.commit_pending_runs(true, None, false) {
                    Ok(()) => {
                        // The bytes are on disk. Drop a retry wait left by
                        // the failed normal pass so the next write is not held.
                        self.disk.clear_write_retry();
                        // The caller is still waiting. A reread blocked on
                        // the rows just committed has to be durable before
                        // this request is marked done. The lane's next pass
                        // is too late: the caller already saw success.
                        if let Err(cause) = self.sync_queued_after_force() {
                            tracing::error!(
                                "Couldn't flush a queued reread for storage_module {}: {cause}",
                                self.id
                            );
                            error = Some(cause.to_string());
                        }
                    }
                    Err(cause) => {
                        tracing::error!(
                            "Couldn't flush packed chunks for storage_module {}: {cause}",
                            self.id
                        );
                        error = Some(cause.to_string());
                    }
                }
            }
            if request.persist
                && let Err(cause) = self.persist_intervals_authoritative(false)
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
        match self.force_sync_pending_chunks() {
            Ok(()) => {
                if let Err(error) = self.persist_intervals_authoritative(false) {
                    tracing::error!(
                        "Couldn't persist intervals for storage module {} on disk lane stop: {error}",
                        self.id
                    );
                }
            }
            Err(error) => {
                tracing::error!(
                    "Couldn't flush storage module {} on disk lane stop: {error}",
                    self.id
                );
            }
        }
        self.service_lane_handoff();
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
    // The waiter gave the disk back. The bytes are not stored. Ingress maps
    // this variant to Overloaded so the sender retries.
    WriteDataChunkError::Backpressure
}

/// One disk slot copied out of the map so the span callback does not borrow it.
#[derive(Clone, Copy)]
struct QueuedOffset {
    offset: PartitionChunkOffset,
    priority: WritePriority,
    enqueued_at: Instant,
}

/// Coalesce key. `Lone` is the offset, so two unassigned slots never join.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SpanGroup {
    Submodule(u32),
    Lone(u32),
}

fn span_command_bytes(start: u32, end: u32, chunk_size: u64) -> u64 {
    u64::from(end.saturating_sub(start).saturating_add(1)).saturating_mul(chunk_size)
}

fn memory_entropy(slot: &SweepSlot) -> bool {
    slot.pending_entropy.is_some() || slot.cached_entropy.is_some()
}

/// True when a queued write still owns an offset in `span`. The pread would
/// pack bytes from before that write.
fn disk_span_waits_on_write(pending: &super::PendingWrites, span: &[QueuedOffset]) -> bool {
    span.iter().any(|slot| {
        pending.contains_key(&slot.offset) || pending.written_ahead.contains_key(&slot.offset)
    })
}

/// Pread bytes of one formed span, holes included. `span` is in offset order.
fn span_queued_bytes(span: &[QueuedOffset], chunk_size: u64) -> u64 {
    let Some(first) = span.first() else {
        return 0;
    };
    let Some(last) = span.last() else {
        return 0;
    };
    span_command_bytes(first.offset.0, last.offset.0, chunk_size)
}

fn span_can_grow(
    start: u32,
    end: u32,
    next: u32,
    chunk_size: u64,
    hole_bytes: u64,
    max_bytes: u64,
) -> bool {
    if next <= end {
        return false;
    }
    let gap_bytes = u64::from(next - end - 1).saturating_mul(chunk_size);
    let bytes = span_command_bytes(start, next, chunk_size);
    gap_bytes <= hole_bytes && bytes <= max_bytes
}

fn clone_error(error: &WriteDataChunkError) -> WriteDataChunkError {
    match error {
        WriteDataChunkError::DataRootNotFound => WriteDataChunkError::DataRootNotFound,
        WriteDataChunkError::WritesPaused => WriteDataChunkError::WritesPaused,
        WriteDataChunkError::Backpressure => WriteDataChunkError::Backpressure,
        WriteDataChunkError::Other(report) => WriteDataChunkError::Other(eyre::eyre!("{report}")),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        thread,
        time::{Duration, Instant},
    };

    use super::{DiskGate, INFLIGHT_DISK_OPS, INFLIGHT_WRITES};
    use irys_types::{
        ConsensusConfig, ConsensusOptions, DataTransactionHeader, DataTransactionHeaderV1, H256,
        LedgerChunkOffset, LedgerChunkRange, NodeConfig, PartitionChunkOffset, StorageSyncConfig,
        TxChunkOffset, UnpackedChunk, ledger_chunk_offset_ii,
    };
    use nodit::interval::ii;

    /// An `Instant` this far in the past. The clock cannot go backwards.
    fn ago(age: Duration) -> Instant {
        Instant::now()
            .checked_sub(age)
            .expect("test age fits in Instant")
    }

    #[test]
    fn write_window_stops_at_the_inflight_limit() {
        let disk = DiskGate::new(true);
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
        let disk = Arc::new(DiskGate::new(true));
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
        assert!(disk.recall_holds());
        assert!(!disk.try_submit_write(INFLIGHT_WRITES, 0));
        assert_eq!(disk.disk_ops.load(Ordering::SeqCst), 1);
        release_tx.send(()).expect("release send");
        recall.join().expect("recall thread");
        assert!(!disk.recall_holds());
        disk.finish_op(0);
        assert_eq!(disk.disk_ops.load(Ordering::SeqCst), 0);
    }

    /// A recall that does not hold the disk is not waited on: other IO
    /// shares the disk with the recall preads and the yields return at once.
    #[test]
    fn shared_disk_recall_does_not_hold_the_disk() {
        let disk = DiskGate::new(false);
        let _recall = disk.begin_recall();
        assert_eq!(disk.recall_waiters.load(Ordering::SeqCst), 1);
        assert!(!disk.recall_holds());
        // No recall hold means the disk is idle for grace purposes and open
        // for new submits while the recall preads run.
        assert!(disk.chunk_disk_idle());
        assert!(disk.disk_available());
        assert!(disk.try_submit_write(INFLIGHT_WRITES, 0));
        assert!(disk.try_submit_read(INFLIGHT_DISK_OPS, 0));
        disk.finish_op(0);
        disk.finish_op(0);
        disk.yield_to_recall();
        assert!(disk.yield_to_recall_until(Instant::now()));
        drop(_recall);
        assert_eq!(disk.recall_waiters.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn write_hold_blocks_entropy_reads() {
        let disk = DiskGate::new(true);
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
        let disk = Arc::new(DiskGate::new(true));
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
        let disk = Arc::new(DiskGate::new(true));
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
        assert!(!disk.recall_holds());
    }

    /// An unlimited sync must not report the batch durable while a recall
    /// flush still owes a window. The owed window writes its own runs. The
    /// next unlimited sync writes the rest, and those bytes stay readable.
    #[test]
    fn unlimited_sync_errors_while_recall_flush_is_owed() -> eyre::Result<()> {
        // One chunk per run, so the owed window cannot swallow the tail.
        let (_tmp, module) = span_fixture_timed("recall_owed_unlimited_sync", 16, 1, 0, 0, 0, 0)?;
        let total = u32::try_from(INFLIGHT_DISK_OPS).expect("window") + 3;
        for offset in 0..total {
            assert!(module.write_chunk(
                PartitionChunkOffset::from(offset),
                vec![u8::try_from(offset + 1).expect("byte"); 32],
                super::super::ChunkType::Data,
            ));
        }
        module.disk.arm_recall_flush();

        let err = module
            .force_sync_pending_chunks()
            .expect_err("unlimited sync reported success while a recall flush is owed");
        assert!(err.to_string().contains("recall flush"), "{err}");
        assert!(module.disk.recall_flush_is_owed());
        assert!(module.has_pending_writes());
        for offset in 0..total {
            assert!(
                !module.is_data_chunk_durable_at(PartitionChunkOffset::from(offset)),
                "offset {offset} was durable before the owed window"
            );
        }

        module.flush_packed_window()?;
        assert!(!module.disk.recall_flush_is_owed());
        let window = u32::try_from(INFLIGHT_DISK_OPS).expect("window");
        for offset in 0..window {
            let at = PartitionChunkOffset::from(offset);
            assert!(
                module.is_data_chunk_durable_at(at),
                "window skipped offset {offset}"
            );
            assert_eq!(
                module.read_chunk_internal(at)?,
                vec![u8::try_from(offset + 1).expect("byte"); 32]
            );
        }
        for offset in window..total {
            assert!(
                !module.is_data_chunk_durable_at(PartitionChunkOffset::from(offset)),
                "offset {offset} was durable before the unlimited sync"
            );
        }

        module.force_sync_pending_chunks()?;
        assert!(!module.has_pending_writes());
        for offset in 0..total {
            let at = PartitionChunkOffset::from(offset);
            assert!(module.is_data_chunk_durable_at(at));
            assert_eq!(
                module.read_chunk_internal(at)?,
                vec![u8::try_from(offset + 1).expect("byte"); 32]
            );
        }
        Ok(())
    }

    #[test]
    fn write_budget_stops_before_five_hundred_ms() {
        let disk = DiskGate::new(true);
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
        let (_tmp, module) = span_fixture("span_order", 8, 8, 8)?;
        insert_slots(&module, &[2, 0, 1], Instant::now());
        assert_eq!(taken_offsets(&module).expect("span"), vec![0, 1, 2]);
        Ok(())
    }

    fn sweep_slot(offset: u32) -> super::SweepSlot {
        sweep_slot_at(offset, Instant::now())
    }

    fn sweep_slot_at(offset: u32, enqueued_at: Instant) -> super::SweepSlot {
        super::SweepSlot {
            group: 1,
            offset: PartitionChunkOffset::from(offset),
            unpacked: Arc::new(vec![0; 32]),
            data_path: Arc::new(Vec::new()),
            path_hash: H256::zero(),
            generation: 0,
            priority: super::WritePriority::Ingress,
            pending_entropy: None,
            cached_entropy: None,
            entropy_epoch: 0,
            byte_len: 32,
            enqueued_at,
        }
    }

    fn insert_slots(module: &super::super::StorageModule, offsets: &[u32], enqueued_at: Instant) {
        let mut queue = module.disk.queue();
        for &offset in offsets {
            queue.insert_slot(sweep_slot_at(offset, enqueued_at));
        }
    }

    fn age_queued(module: &super::super::StorageModule, enqueued_at: Instant) {
        let mut queue = module.disk.queue();
        for slot in queue.slots.values_mut() {
            slot.enqueued_at = enqueued_at;
        }
    }

    fn taken_offsets(module: &super::super::StorageModule) -> Option<Vec<u32>> {
        module
            .take_disk_span()
            .map(|span| span.iter().map(|slot| slot.offset.0).collect())
    }

    #[test]
    fn young_short_span_waits_out_the_entropy_read_grace() -> eyre::Result<()> {
        let grace = Duration::from_millis(80);
        let (_tmp, module) =
            span_fixture_grace("span_grace", 8, 8, 0, grace.as_millis() as u64, 0)?;
        let now = Instant::now();
        insert_slots(&module, &[0, 1], now);
        assert!(
            taken_offsets(&module).is_none(),
            "a short span younger than the grace stays queued"
        );
        age_queued(&module, ago(grace + Duration::from_millis(1)));
        assert_eq!(
            taken_offsets(&module).expect("an aged short span is issued"),
            vec![0, 1]
        );
        Ok(())
    }

    #[test]
    fn span_at_the_batch_target_is_issued_while_young() -> eyre::Result<()> {
        let (_tmp, module) = span_fixture_grace("span_target", 8, 8, 0, 80, 0)?;
        insert_slots(&module, &(0..8).collect::<Vec<_>>(), Instant::now());
        assert_eq!(
            taken_offsets(&module)
                .expect("a span at the batch target does not wait")
                .len(),
            8
        );
        Ok(())
    }

    #[test]
    fn packing_span_is_issued_while_young() -> eyre::Result<()> {
        let (_tmp, module) = span_fixture_grace("span_pack", 8, 8, 0, 80, 0)?;
        {
            let mut queue = module.disk.queue();
            let mut slot = sweep_slot_at(0, Instant::now());
            slot.priority = super::WritePriority::Packing;
            queue.insert_slot(slot);
        }
        assert!(
            module.take_disk_span().is_some(),
            "a packing span does not wait out the ingest grace"
        );
        Ok(())
    }

    #[test]
    fn lone_chunk_becomes_eligible_when_the_grace_ends() -> eyre::Result<()> {
        let grace = Duration::from_millis(80);
        let (_tmp, module) = span_fixture_grace("span_lone", 8, 8, 0, grace.as_millis() as u64, 0)?;
        insert_slots(&module, &[3], Instant::now());
        let remain = module
            .read_grace_remaining()
            .expect("a queued span has a grace");
        assert!(remain > Duration::from_millis(40), "remain {remain:?}");
        assert!(remain <= grace);
        assert!(taken_offsets(&module).is_none());
        age_queued(&module, ago(grace + Duration::from_millis(1)));
        assert_eq!(module.read_grace_remaining(), Some(Duration::ZERO));
        assert_eq!(taken_offsets(&module).expect("aged lone chunk"), vec![3]);
        Ok(())
    }

    #[test]
    fn adjacent_chunks_inside_the_grace_are_one_entropy_read() -> eyre::Result<()> {
        let grace = Duration::from_millis(80);
        let (_tmp, module) =
            span_fixture_grace("span_burst", 8, 8, 0, grace.as_millis() as u64, 0)?;
        insert_slots(&module, &[0], Instant::now());
        assert!(!module.sweep_one());
        assert_eq!(module.disk.entropy_preads.load(Ordering::SeqCst), 0);
        insert_slots(&module, &[1], Instant::now());
        assert!(!module.sweep_one());
        assert_eq!(module.disk.entropy_preads.load(Ordering::SeqCst), 0);
        age_queued(&module, ago(grace + Duration::from_millis(1)));
        assert!(module.sweep_one());
        assert_eq!(module.disk.entropy_preads.load(Ordering::SeqCst), 1);
        assert_eq!(module.disk.entropy_pread_slots.load(Ordering::SeqCst), 2);
        assert_eq!(module.disk.entropy_pread_bytes.load(Ordering::SeqCst), 64);
        assert_eq!(module.sweep_slot_count_for_test(), 0);
        Ok(())
    }

    #[test]
    fn recall_preempts_an_aged_entropy_span() -> eyre::Result<()> {
        let (_tmp, module) = span_fixture_grace("span_recall", 8, 8, 0, 80, 0)?;
        insert_slots(&module, &[0], ago(Duration::from_millis(81)));
        let hold = module.disk.begin_recall();
        assert!(!module.sweep_one());
        assert_eq!(module.disk.entropy_preads.load(Ordering::SeqCst), 0);
        assert_eq!(module.sweep_slot_count_for_test(), 1);
        drop(hold);
        assert!(module.sweep_one());
        assert_eq!(module.disk.entropy_preads.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[test]
    fn young_lone_chunk_drains_inside_the_grace() -> eyre::Result<()> {
        let grace = Duration::from_millis(40);
        let (_tmp, module) =
            span_fixture_grace("span_drain", 8, 8, 0, grace.as_millis() as u64, 0)?;
        insert_slots(&module, &[0], Instant::now());
        let started = Instant::now();
        module.drive_entropy_queue_for_test();
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(20),
            "drain took {elapsed:?}, before the grace"
        );
        assert_eq!(module.disk.entropy_preads.load(Ordering::SeqCst), 1);
        assert_eq!(module.sweep_slot_count_for_test(), 0);
        Ok(())
    }

    #[test]
    fn reorder_grace_covers_one_scan_of_full_runs() {
        let small = super::reorder_grace(32);
        let production = super::reorder_grace(256 * 1024);
        assert!(small > Duration::from_millis(50));
        assert!(small < Duration::from_millis(200));
        assert!(production > Duration::from_secs(1));
        assert!(production < Duration::from_secs(2));
    }

    #[test]
    fn due_reads_issue_the_lower_offset_first() -> eyre::Result<()> {
        let (_tmp, module) = span_fixture("span_rank", 8, 8, 1)?;
        insert_slots(&module, &[0, 3, 4, 5], Instant::now());
        module.disk.arm_recall_flush();
        let offsets = taken_offsets(&module).expect("span");
        module.disk.clear_recall_flush();
        assert_eq!(offsets, vec![0]);
        Ok(())
    }

    #[test]
    fn young_short_span_is_issued_in_offset_order() -> eyre::Result<()> {
        let (_tmp, module) = span_fixture("span_young", 8, 8, 1)?;
        insert_slots(&module, &[0, 3, 4, 5], Instant::now());
        module.disk.occupy_for_test();
        assert_eq!(
            taken_offsets(&module).expect("a short span is eligible while the disk is busy"),
            vec![0]
        );
        assert_eq!(
            taken_offsets(&module).expect("the later span stays queued"),
            vec![3, 4, 5]
        );
        module.disk.release_for_test();
        Ok(())
    }

    #[test]
    fn full_span_is_read_while_young() -> eyre::Result<()> {
        let (_tmp, module) = span_fixture("span_full", 8, 8, 1)?;
        insert_slots(&module, &(0..8).collect::<Vec<_>>(), Instant::now());
        assert_eq!(
            taken_offsets(&module).expect("span"),
            (0..8).collect::<Vec<_>>()
        );
        Ok(())
    }

    #[test]
    fn full_span_stays_ahead_of_an_aged_short_span() -> eyre::Result<()> {
        let (_tmp, module) = span_fixture("span_short_first", 16, 8, 1)?;
        let now = Instant::now();
        insert_slots(&module, &(0..8).collect::<Vec<_>>(), now);
        insert_slots(&module, &[10], now);
        // Offset order keeps the full span ahead of the later short span.
        // The short span is still eligible while the disk is busy.
        module.disk.occupy_for_test();
        assert_eq!(
            taken_offsets(&module).expect("full"),
            (0..8).collect::<Vec<_>>()
        );
        assert_eq!(taken_offsets(&module).expect("short"), vec![10]);
        module.disk.release_for_test();
        Ok(())
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
        span_fixture_grace(prefix, chunks, max_chunks, hole_chunks, 0, 0)
    }

    fn span_fixture_grace(
        prefix: &str,
        chunks: u64,
        max_chunks: u64,
        hole_chunks: u64,
        grace_ms: u64,
        cache_bytes: u64,
    ) -> eyre::Result<(
        irys_testing_utils::utils::tempfile::TempDir,
        super::super::StorageModule,
    )> {
        span_fixture_timed(
            prefix,
            chunks,
            max_chunks,
            hole_chunks,
            grace_ms,
            cache_bytes,
            0,
        )
    }

    fn span_fixture_timed(
        prefix: &str,
        chunks: u64,
        max_chunks: u64,
        hole_chunks: u64,
        grace_ms: u64,
        cache_bytes: u64,
        durable_ms: u64,
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
                entropy_read_grace_ms: grace_ms,
                entropy_read_batch_bytes: chunk_size * max_chunks,
                entropy_readahead_bytes: 0,
                entropy_readahead_cache_bytes: cache_bytes,
                drop_recall_page_cache: false,
                recall_direct_io: false,
                recall_io_priority: true,
                intervals_persist_secs: 0,
                index_wal_backstop_ms: irys_types::DEFAULT_INDEX_WAL_BACKSTOP_MS,
                tx_path_derive_threads: irys_types::DEFAULT_TX_PATH_DERIVE_THREADS,
                min_durable_interval_ms: durable_ms,
                durable_headroom_permille: irys_types::DEFAULT_DURABLE_HEADROOM_PERMILLE,
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

    fn entropy_on_disk(module: &super::super::StorageModule) -> eyre::Result<()> {
        let offset = PartitionChunkOffset::from(0);
        assert!(module.write_chunk(offset, vec![0; 32], super::super::ChunkType::Entropy));
        module.sync_pending_chunks_inner(true)?;
        Ok(())
    }

    fn slot_holding_zero_pad(unpacked: u8) -> super::SweepSlot {
        let mut slot = sweep_slot(0);
        slot.unpacked = Arc::new(vec![unpacked; 32]);
        slot.cached_entropy = Some(vec![0; 32]);
        slot.byte_len = 32;
        slot.generation = 0;
        slot
    }

    fn pack_queued_slot(
        module: &super::super::StorageModule,
        slot: super::SweepSlot,
    ) -> Result<(), super::super::WriteDataChunkError> {
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let offset = slot.offset;
        let generation = slot.generation;
        module.disk.queue().groups.insert(
            slot.group,
            super::SweepGroup {
                remaining: 1,
                failed: None,
                waited: true,
                notify: Some(tx),
            },
        );
        module
            .pending_writes
            .write()
            .unwrap()
            .occupancy
            .insert(offset, generation);
        module.pack_one(slot);
        rx.try_recv().expect("the slot must finish")
    }

    fn arm_slot_group(
        module: &super::super::StorageModule,
        slot: super::SweepSlot,
    ) -> tokio::sync::oneshot::Receiver<Result<(), super::super::WriteDataChunkError>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut queue = module.disk.queue();
            queue.groups.insert(
                slot.group,
                super::SweepGroup {
                    remaining: 1,
                    failed: None,
                    waited: true,
                    notify: Some(tx),
                },
            );
        }
        module
            .pending_writes
            .write()
            .unwrap()
            .occupancy
            .insert(slot.offset, slot.generation);
        module.disk.queue().insert_slot(slot);
        rx
    }

    /// A later occupy at the same offset must finish the group it displaces.
    /// The old oneshot and its unpacked length belong to that group.
    #[test]
    fn replacing_a_queued_sweep_slot_finishes_the_old_group() -> eyre::Result<()> {
        let (_tmp, module) = span_fixture("sweep_replace", 4, 4, 0)?;
        let mut slot = sweep_slot(0);
        slot.byte_len = 32;
        let mut rx = arm_slot_group(&module, slot);
        module.index_write_generation.fetch_add(1, Ordering::SeqCst);
        {
            let mut pending = module.pending_writes.write().unwrap();
            pending.occupancy.clear();
            // Occupy counted the original slot and the replacement.
            pending.queued_unpacked_bytes = 64;
        }
        let occupied = super::OccupiedChunk {
            unpacked: Arc::new(vec![1; 32]),
            data_path: Arc::new(Vec::new()),
            path_hash: H256::zero(),
            priority: super::WritePriority::Ingress,
            waited: false,
            notify: None,
            selected: vec![(PartitionChunkOffset::from(0), 1, None, None, 0)],
        };
        module
            .push_occupied(vec![occupied])
            .expect("replacement queued");
        module.fail_queued_sweeps();
        match rx.try_recv() {
            Ok(Err(super::super::WriteDataChunkError::Backpressure)) => {}
            other => eyre::bail!("original sweep group was not finished: {other:?}"),
        }
        assert!(
            !module.disk.queue().groups.contains_key(&1),
            "original group is still queued"
        );
        assert_eq!(
            module.pending_writes.read().unwrap().queued_unpacked_bytes,
            0,
            "replaced slot bytes stayed in the pending total"
        );
        Ok(())
    }

    #[test]
    fn copied_pad_is_not_packed_over_written_ahead() -> eyre::Result<()> {
        let (_tmp, module) = span_fixture("stale_pad_ahead", 4, 4, 0)?;
        entropy_on_disk(&module)?;
        let offset = PartitionChunkOffset::from(0);
        {
            let mut pending = module.pending_writes.write().unwrap();
            pending.written_ahead.insert(
                offset,
                super::super::WrittenAhead {
                    bytes: vec![0xAB; 32],
                    chunk_type: super::super::ChunkType::Entropy,
                    priority: super::WritePriority::Packing,
                    queued_at: Instant::now(),
                    pwrite_at: None,
                    parked_at: Instant::now(),
                },
            );
        }
        let finished = pack_queued_slot(&module, slot_holding_zero_pad(0x11));
        assert!(
            matches!(
                finished,
                Err(super::super::WriteDataChunkError::Backpressure)
            ),
            "{finished:?}"
        );
        let pending = module.pending_writes.read().unwrap();
        assert!(
            pending.written_ahead.contains_key(&offset),
            "commit_packed dropped a written_ahead entry it did not write"
        );
        assert!(
            pending.get(&offset).is_none(),
            "packed the copied pad into pending: {:?}",
            pending
                .get(&offset)
                .map(|(bytes, kind)| (kind, bytes.first().copied()))
        );
        drop(pending);
        // `pack_one` can refuse before `commit_packed`. This call is the
        // commit itself: occupancy matches, and the entry must stay.
        module
            .pending_writes
            .write()
            .unwrap()
            .occupancy
            .insert(offset, 0);
        let mut disk_compare = Duration::ZERO;
        let refused = module.commit_packed(
            offset,
            vec![0x11; 32],
            0,
            super::WritePriority::Ingress,
            32,
            (H256::zero(), Vec::new()),
            true,
            module.entropy_write_epoch.load(Ordering::SeqCst),
            None,
            &[],
            &mut disk_compare,
        );
        assert!(
            matches!(
                refused,
                Err(super::super::WriteDataChunkError::Backpressure)
            ),
            "{refused:?}"
        );
        let pending = module.pending_writes.read().unwrap();
        assert!(pending.written_ahead.contains_key(&offset));
        assert!(pending.get(&offset).is_none());
        Ok(())
    }

    #[test]
    fn copied_pad_is_not_packed_over_pending_entropy() -> eyre::Result<()> {
        let (_tmp, module) = span_fixture("stale_pad_pending", 4, 4, 0)?;
        entropy_on_disk(&module)?;
        let offset = PartitionChunkOffset::from(0);
        {
            let mut pending = module.pending_writes.write().unwrap();
            pending.insert(offset, (vec![0xAB; 32], super::super::ChunkType::Entropy));
        }
        let finished = pack_queued_slot(&module, slot_holding_zero_pad(0x11));
        assert!(
            matches!(
                finished,
                Err(super::super::WriteDataChunkError::Backpressure)
            ),
            "{finished:?}"
        );
        let pending = module.pending_writes.read().unwrap();
        let (bytes, kind) = pending.get(&offset).expect("the new entropy stays queued");
        assert_eq!(*kind, super::super::ChunkType::Entropy);
        assert_eq!(bytes.first().copied(), Some(0xAB));
        Ok(())
    }

    /// The sweep already read `0x11`. A newer entropy durable point lands
    /// `0x22` and clears `pending` and `written_ahead` before the XOR. The
    /// sweep must not queue `Data` packed with the old pad (`0x11 ^ 0x01`).
    #[test]
    fn pread_does_not_pack_entropy_replaced_before_submit() -> eyre::Result<()> {
        let (_tmp, module) = span_fixture("stale_pread", 4, 4, 0)?;
        let offset = PartitionChunkOffset::from(0);
        assert!(module.write_chunk(offset, vec![0x11; 32], super::super::ChunkType::Entropy));
        module.sync_pending_chunks_inner(true)?;
        module.pause_next_sync_at(super::super::SyncPausePoint::AfterEntropyPread);
        let mut slot = sweep_slot(0);
        slot.unpacked = Arc::new(vec![0x01; 32]);
        slot.byte_len = 32;
        let mut rx = arm_slot_group(&module, slot);
        std::thread::scope(|scope| {
            let sweep = scope.spawn(|| module.sweep_one());
            assert!(
                module.wait_for_sync_pause(Duration::from_secs(5)),
                "sweep did not pause after the entropy pread"
            );
            assert!(module.write_chunk(offset, vec![0x22; 32], super::super::ChunkType::Entropy));
            module
                .sync_pending_chunks_inner(true)
                .expect("replacement sync");
            {
                let pending = module.pending_writes.read().unwrap();
                assert!(
                    pending.get(&offset).is_none(),
                    "replacement entropy is still pending"
                );
                assert!(
                    !pending.written_ahead.contains_key(&offset),
                    "replacement entropy is still write-ahead"
                );
            }
            assert_eq!(
                module
                    .intervals
                    .read()
                    .unwrap()
                    .get_at_point(offset)
                    .copied(),
                Some(super::super::ChunkType::Entropy)
            );
            module.release_sync_pause();
            assert!(sweep.join().expect("sweep thread"), "sweep did not read");
        });
        let pending_byte = module
            .pending_writes
            .read()
            .unwrap()
            .get(&offset)
            .map(|(bytes, kind)| (*kind, bytes.first().copied()));
        match rx.try_recv() {
            Ok(Err(super::super::WriteDataChunkError::Backpressure)) => {}
            other => eyre::bail!(
                "stale entropy pread was packed: result {other:?} pending {pending_byte:?}"
            ),
        }
        assert_ne!(
            pending_byte,
            Some((super::super::ChunkType::Data, Some(0x10))),
            "packed the entropy from before the replacement"
        );
        Ok(())
    }

    /// No newer entropy pwrite. The sweep packs the pad it just read.
    #[test]
    fn pread_packs_the_entropy_it_read() -> eyre::Result<()> {
        let (_tmp, module) = span_fixture("fresh_pread", 4, 4, 0)?;
        let offset = PartitionChunkOffset::from(0);
        assert!(module.write_chunk(offset, vec![0x11; 32], super::super::ChunkType::Entropy));
        module.sync_pending_chunks_inner(true)?;
        let mut slot = sweep_slot(0);
        slot.unpacked = Arc::new(vec![0x01; 32]);
        slot.byte_len = 32;
        let mut rx = arm_slot_group(&module, slot);
        assert!(module.sweep_one(), "sweep did not read");
        rx.try_recv().expect("slot finished").expect("pack");
        let pending = module.pending_writes.read().unwrap();
        let (bytes, kind) = pending.get(&offset).expect("packed data");
        assert_eq!(*kind, super::super::ChunkType::Data);
        assert_eq!(bytes.first().copied(), Some(0x10));
        Ok(())
    }

    /// A read-ahead hole from before an entropy pwrite must not stay cached.
    /// The same buffer is cached when the epoch still matches.
    #[test]
    fn remember_readahead_drops_entropy_from_before_a_later_write() -> eyre::Result<()> {
        let (_tmp, module) = span_fixture_grace("stale_readahead", 4, 4, 0, 0, 32 * 4)?;
        let offset = PartitionChunkOffset::from(0);
        assert!(module.write_chunk(offset, vec![0x11; 32], super::super::ChunkType::Entropy));
        module.sync_pending_chunks_inner(true)?;
        let epoch = module.entropy_write_epoch.load(Ordering::SeqCst);
        let buf = vec![0x11; 32];
        let verdicts = [super::super::chunk_crc::CrcVerdict::Match];
        module.remember_readahead(offset, &buf, std::iter::empty(), &verdicts, epoch);
        assert_eq!(
            module.entropy_cache_offsets_for_test(),
            vec![0],
            "a current entropy read was not cached"
        );
        module.entropy_cache.lock().unwrap().clear();
        assert!(module.write_chunk(offset, vec![0x22; 32], super::super::ChunkType::Entropy));
        module.sync_pending_chunks_inner(true)?;
        module.remember_readahead(offset, &buf, std::iter::empty(), &verdicts, epoch);
        assert!(
            module.entropy_cache_offsets_for_test().is_empty(),
            "cached entropy from before the later pwrite"
        );
        Ok(())
    }

    /// Taking the front of a fragmented queue must not rescan the tail.
    /// Thirty-two one-slot takes on a 16× longer queue stay well under 16× the cost.
    #[test]
    fn fragmented_span_take_does_not_rescan_the_tail() -> eyre::Result<()> {
        fn queue_fragmented(
            prefix: &str,
            slots: u32,
        ) -> eyre::Result<(
            irys_testing_utils::utils::tempfile::TempDir,
            super::super::StorageModule,
        )> {
            let last = u64::from(slots) * 2;
            let queued = span_fixture(prefix, last + 1, 1, 0)?;
            {
                let mut queue = queued.1.disk.queue();
                for index in 0..slots {
                    queue.insert_slot(sweep_slot(index * 2));
                }
            }
            Ok(queued)
        }
        fn take_front(module: &super::super::StorageModule, takes: usize) -> (Duration, u64) {
            module.disk.span_walk_slots.store(0, Ordering::SeqCst);
            let started = Instant::now();
            for _ in 0..takes {
                let span = module.take_disk_span().expect("fragmented span");
                assert_eq!(span.len(), 1, "hole of 0 must not coalesce");
            }
            (
                started.elapsed(),
                module.disk.span_walk_slots.load(Ordering::SeqCst),
            )
        }

        let (_small_dir, small) = queue_fragmented("span_scale_small", 2_000)?;
        let (_large_dir, large) = queue_fragmented("span_scale_large", 32_000)?;
        let (small_time, small_visits) = take_front(&small, 32);
        let (large_time, large_visits) = take_front(&large, 32);
        eprintln!(
            "fragmented take 32 spans: queue 2000 {small_time:?} visits {small_visits}, queue 32000 {large_time:?} visits {large_visits}"
        );
        // Each take looks at the closed span plus the next slot that ends it.
        assert!(
            large_visits < 1_000,
            "front span take walked the backlog: 2000 slots {small_visits} visits, 32000 slots {large_visits} visits"
        );
        assert!(large_visits >= 32, "span walk counter did not move");
        assert!(
            large_time < small_time.saturating_mul(8) || large_time < Duration::from_millis(80),
            "front span take rescanned the queue: 2000 slots {small_time:?}, 32000 slots {large_time:?}"
        );
        Ok(())
    }

    /// Newest-first removal must not scan the FIFO.
    #[test]
    fn entropy_cache_newest_remove_scales() {
        fn remove_newest_first(pads: u32) -> Duration {
            let mut cache = super::EntropyPadCache::default();
            let cap = u64::from(pads) * 32;
            for index in 0..pads {
                assert!(cache.insert(PartitionChunkOffset::from(index), vec![1; 32], 1, cap, 0,));
            }
            let started = Instant::now();
            for index in (0..pads).rev() {
                assert!(cache.take(PartitionChunkOffset::from(index), 1).is_some());
            }
            started.elapsed()
        }

        let small = remove_newest_first(2_000);
        let large = remove_newest_first(8_000);
        eprintln!("entropy cache newest-first remove: 2000 pads {small:?}, 8000 pads {large:?}");
        assert!(
            large < small.saturating_mul(8) || large < Duration::from_millis(50),
            "cache remove scanned the FIFO: 2000 pads {small:?}, 8000 pads {large:?}"
        );
    }

    /// A reinserted pad is the newest. Eviction must not treat the stale
    /// FIFO row as the live pad.
    #[test]
    fn entropy_cache_reinsert_evicts_the_oldest_live_pad() {
        let mut cache = super::EntropyPadCache::default();
        let cap = 32 * 2;
        for index in 0..2_u32 {
            assert!(cache.insert(PartitionChunkOffset::from(index), vec![1; 32], 1, cap, 0,));
        }
        assert!(cache.insert(PartitionChunkOffset::from(0), vec![2; 32], 1, cap, 0));
        assert!(cache.take(PartitionChunkOffset::from(0), 1).is_some());
        assert!(cache.insert(PartitionChunkOffset::from(2), vec![3; 32], 1, cap, 0));
        assert!(cache.insert(PartitionChunkOffset::from(3), vec![4; 32], 1, cap, 0));
        assert!(
            cache.take(PartitionChunkOffset::from(1), 1).is_none(),
            "the stale front row evicted a newer pad, or the oldest live pad survived"
        );
        assert!(cache.take(PartitionChunkOffset::from(2), 1).is_some());
        assert!(cache.take(PartitionChunkOffset::from(3), 1).is_some());
    }

    #[test]
    fn rewrite_fails_a_queued_pad_before_fold() -> eyre::Result<()> {
        let (_tmp, module) = span_fixture("stale_pad_rewrite", 4, 4, 0)?;
        entropy_on_disk(&module)?;
        let offset = PartitionChunkOffset::from(0);
        let mut rx = arm_slot_group(&module, slot_holding_zero_pad(0x11));
        assert!(module.write_chunk(offset, vec![0xAB; 32], super::super::ChunkType::Entropy));
        module.sync_pending_chunks_inner(true)?;
        module.fold_memory_entropy();
        let pending = module.pending_writes.read().unwrap();
        if let Some((bytes, kind)) = pending.get(&offset) {
            assert_ne!(
                (*kind, bytes.first().copied()),
                (super::super::ChunkType::Data, Some(0x11)),
                "packed with the pre-rewrite pad"
            );
        }
        drop(pending);
        let finished = rx
            .try_recv()
            .expect("the rewrite must fail the queued slot");
        assert!(
            matches!(
                finished,
                Err(super::super::WriteDataChunkError::Backpressure)
            ),
            "{finished:?}"
        );
        Ok(())
    }

    /// One indexed chunk. Bytes are `0x01`, so packing entropy `0x11` yields `0x10`.
    fn index_unpacked_byte(module: &super::super::StorageModule) -> eyre::Result<UnpackedChunk> {
        let data_root = H256::repeat_byte(0x42);
        let header = DataTransactionHeader::V1(irys_types::DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                data_root,
                data_size: 32,
                ..Default::default()
            },
            metadata: irys_types::DataTransactionMetadata::new(),
        });
        module.index_transaction_data(
            &header,
            &vec![5, 6, 7, 8],
            LedgerChunkRange(ledger_chunk_offset_ii!(0, 0)),
            0,
        )?;
        Ok(UnpackedChunk {
            data_root,
            data_size: 32,
            data_path: vec![4, 3, 2, 1].into(),
            bytes: vec![0x01; 32].into(),
            tx_offset: TxChunkOffset::from(0),
        })
    }

    fn cache_module(
        prefix: &str,
    ) -> eyre::Result<(
        irys_testing_utils::utils::tempfile::TempDir,
        super::super::StorageModule,
        UnpackedChunk,
    )> {
        let (tmp, module) = span_fixture_grace(prefix, 4, 4, 0, 0, 32 * 4)?;
        let chunk = index_unpacked_byte(&module)?;
        Ok((tmp, module, chunk))
    }

    /// Durable entropy `byte`, then the same bytes in the read-ahead cache.
    fn cache_durable_entropy(module: &super::super::StorageModule, byte: u8) -> eyre::Result<()> {
        let offset = PartitionChunkOffset::from(0);
        let bytes = vec![byte; 32];
        assert!(module.write_chunk(offset, bytes.clone(), super::super::ChunkType::Entropy));
        module.sync_pending_chunks_inner(true)?;
        let epoch = module.entropy_write_epoch.load(Ordering::SeqCst);
        let verdicts = [super::super::chunk_crc::CrcVerdict::Match];
        module.remember_readahead(offset, &bytes, std::iter::empty(), &verdicts, epoch);
        assert_eq!(
            module.entropy_cache_offsets_for_test(),
            vec![0],
            "durable entropy was not cached"
        );
        Ok(())
    }

    fn pending_head(
        module: &super::super::StorageModule,
    ) -> Option<(super::super::ChunkType, Option<u8>)> {
        module
            .pending_writes
            .read()
            .unwrap()
            .get(&PartitionChunkOffset::from(0))
            .map(|(bytes, kind)| (*kind, bytes.first().copied()))
    }

    struct StopLane {
        module: Arc<super::super::StorageModule>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl StopLane {
        fn start(module: Arc<super::super::StorageModule>) -> eyre::Result<Self> {
            let handle = module
                .spawn_disk_lane()?
                .ok_or_else(|| eyre::eyre!("disk lane did not start"))?;
            Ok(Self {
                module,
                handle: Some(handle),
            })
        }
    }

    impl Drop for StopLane {
        fn drop(&mut self) {
            self.module.stop_disk_lane();
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    /// Spin until the first commit has failed and armed a retry.
    /// Panics when a second commit lands first: that is the tight retry.
    fn wait_for_failed_commit(module: &super::super::StorageModule) -> Duration {
        let started = Instant::now();
        loop {
            let calls = module.disk.commit_calls();
            if calls > 1 {
                panic!("lane retried a failed pwrite with no backoff: calls={calls}");
            }
            if calls == 1 && module.disk.write_retry_pending() {
                return module
                    .disk
                    .write_retry_remaining()
                    .expect("a pending retry has time left");
            }
            // The injected error builds a backtrace before the commit returns.
            // A full nextest run can spend more than 10s inside that backtrace
            // while the retry is still unarmed. The tight retry is a second
            // commit after that return, not a commit that is still inside the
            // first failure.
            if started.elapsed() > Duration::from_secs(30) {
                panic!(
                    "failed pwrite did not arm a retry: calls={calls} durable={} idle={} ready={}",
                    module.is_data_chunk_durable_at(PartitionChunkOffset::from(0)),
                    module.disk.chunk_disk_idle(),
                    module.pending_run_ready()
                );
            }
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// A failed `pwrite` keeps the bytes queued. The lane does not commit
    /// again until the retry instant, including after a wake. It does commit
    /// once that instant has passed.
    #[test]
    fn failed_pwrite_waits_out_backoff_before_the_next_commit() -> eyre::Result<()> {
        let (_tmp, module) = span_fixture_timed("pwrite_backoff", 4, 4, 0, 0, 0, 0)?;
        let module = Arc::new(module);
        let lane = StopLane::start(Arc::clone(&module))?;
        module.disk.fail_next_pwrites(1);
        let offset = PartitionChunkOffset::from(0);
        assert!(module.write_chunk(offset, vec![0x11; 32], super::super::ChunkType::Data));
        let remain = wait_for_failed_commit(&module);
        module.wake_disk_lane();
        thread::sleep(remain / 2);
        assert_eq!(
            module.disk.commit_calls(),
            1,
            "lane committed again before the backoff"
        );
        assert!(
            !module.is_data_chunk_durable_at(offset),
            "a failed pwrite became durable during the backoff"
        );
        let started = Instant::now();
        loop {
            if module.disk.commit_calls() >= 2 && module.is_data_chunk_durable_at(offset) {
                break;
            }
            if started.elapsed() > Duration::from_millis(1500) {
                panic!(
                    "lane did not commit after the backoff: calls={} durable={}",
                    module.disk.commit_calls(),
                    module.is_data_chunk_durable_at(offset)
                );
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            module.disk.write_retry_remaining().is_none(),
            "a successful write left a retry deadline"
        );
        drop(lane);
        Ok(())
    }

    /// `force_sync` writes the failed bytes at once and clears the deadline,
    /// so the next packed write does not wait out the old backoff.
    #[test]
    fn force_sync_commits_a_failed_pwrite_before_the_backoff() -> eyre::Result<()> {
        let (_tmp, module) = span_fixture_timed("pwrite_force", 4, 4, 0, 0, 0, 0)?;
        let module = Arc::new(module);
        let lane = StopLane::start(Arc::clone(&module))?;
        module.disk.fail_next_pwrites(1);
        let offset = PartitionChunkOffset::from(0);
        assert!(module.write_chunk(offset, vec![0x11; 32], super::super::ChunkType::Data));
        let remain = wait_for_failed_commit(&module);
        assert!(
            remain > Duration::from_millis(200),
            "backoff was already almost over: {remain:?}"
        );
        let retry_at = Instant::now() + remain;
        let started = Instant::now();
        module.force_sync_pending_chunks()?;
        let elapsed = started.elapsed();
        assert!(
            elapsed < remain && Instant::now() < retry_at,
            "force waited out the backoff: elapsed={elapsed:?} remain={remain:?}"
        );
        assert!(
            elapsed < Duration::from_millis(800),
            "force took {elapsed:?}"
        );
        assert!(
            module.is_data_chunk_durable_at(offset),
            "force did not make the failed chunk durable"
        );
        assert_eq!(
            module.disk.commit_calls(),
            2,
            "force did not commit the failed run once"
        );
        assert!(
            module.disk.write_retry_remaining().is_none(),
            "force left the retry deadline set"
        );
        let next = PartitionChunkOffset::from(1);
        assert!(module.write_chunk(next, vec![0x22; 32], super::super::ChunkType::Data));
        let started = Instant::now();
        loop {
            if module.disk.commit_calls() >= 3 && module.is_data_chunk_durable_at(next) {
                break;
            }
            if Instant::now() >= retry_at || started.elapsed() > Duration::from_millis(500) {
                panic!(
                    "the next write waited on the old deadline: calls={} durable={} elapsed={:?}",
                    module.disk.commit_calls(),
                    module.is_data_chunk_durable_at(next),
                    started.elapsed()
                );
            }
            thread::sleep(Duration::from_millis(5));
        }
        drop(lane);
        Ok(())
    }

    struct ReleasePause<'a>(&'a super::DiskGate);

    impl Drop for ReleasePause<'_> {
        fn drop(&mut self) {
            self.0.entropy_pack_pause.release();
        }
    }

    /// `during` runs while the copied pad is off the sweep queue.
    fn ingress_during_pause(
        module: &Arc<super::super::StorageModule>,
        chunk: &UnpackedChunk,
        pause: super::EntropyPackPause,
        during: impl FnOnce(),
    ) -> Result<(), super::super::WriteDataChunkError> {
        module.disk.entropy_pack_pause.arm(pause);
        thread::scope(|scope| {
            let writing = Arc::clone(module);
            let chunk = chunk.clone();
            let handle = scope.spawn(move || writing.write_data_chunk(&chunk));
            let release = ReleasePause(&module.disk);
            assert!(
                module
                    .disk
                    .entropy_pack_pause
                    .wait_arrived(Duration::from_secs(5)),
                "ingress did not reach {pause:?}"
            );
            assert_eq!(
                module.sweep_slot_count_for_test(),
                0,
                "the copied pad is still queued, so a write can cancel it"
            );
            during();
            drop(release);
            handle.join().expect("ingress thread")
        })
    }

    fn assert_old_pad_not_packed(
        result: Result<(), super::super::WriteDataChunkError>,
        module: &super::super::StorageModule,
        context: &str,
    ) {
        let head = pending_head(module);
        assert!(
            matches!(result, Err(super::super::WriteDataChunkError::Backpressure)),
            "{context}: {result:?} pending {head:?}"
        );
        assert_ne!(
            head,
            Some((super::super::ChunkType::Data, Some(0x10))),
            "{context}: packed the entropy copied before the replacement"
        );
    }

    fn assert_fresh_pad_packed(module: &super::super::StorageModule) {
        let head = pending_head(module);
        assert_eq!(
            head,
            Some((super::super::ChunkType::Data, Some(0x10))),
            "fresh entropy did not pack: {head:?}"
        );
    }

    /// The cache still holds the pread pad after the epoch moves. The file
    /// already has the replacement bytes: the bump is published before the
    /// cache invalidate. Occupy must not pack the old pad.
    #[test]
    fn cached_pad_is_not_packed_after_the_epoch_moves() -> eyre::Result<()> {
        let (_tmp, module, chunk) = cache_module("cache_epoch_gap")?;
        cache_durable_entropy(&module, 0x11)?;
        overwrite_chunk_file(&module, PartitionChunkOffset::from(0), 0x22)?;
        let stored = module.entropy_write_epoch.load(Ordering::SeqCst);
        module.entropy_write_epoch.fetch_add(1, Ordering::SeqCst);
        assert_eq!(
            module.entropy_cache_offsets_for_test(),
            vec![0],
            "the epoch bump dropped the pad before occupy"
        );
        let wrote = module.write_data_chunk(&chunk);
        assert_old_pad_not_packed(wrote, &module, "epoch moved before the cache take");
        assert!(
            stored < module.entropy_write_epoch.load(Ordering::SeqCst),
            "the test did not move the epoch"
        );
        Ok(())
    }

    /// Occupy has the cached pad and has not queued it. A durable point then
    /// replaces the entropy. The lane is down, so this thread syncs.
    #[test]
    fn cached_pad_is_not_packed_after_a_durable_point_without_a_lane() -> eyre::Result<()> {
        let (_tmp, module, chunk) = cache_module("cache_durable_down")?;
        cache_durable_entropy(&module, 0x11)?;
        let module = Arc::new(module);
        let offset = PartitionChunkOffset::from(0);
        let wrote = ingress_during_pause(
            &module,
            &chunk,
            super::EntropyPackPause::AfterEntropyCopied,
            || {
                assert!(module.write_chunk(
                    offset,
                    vec![0x22; 32],
                    super::super::ChunkType::Entropy
                ));
                module
                    .sync_pending_chunks_inner(true)
                    .expect("replacement sync");
                assert!(
                    pending_head(&module).is_none(),
                    "replacement is still pending"
                );
                assert!(
                    !module
                        .pending_writes
                        .read()
                        .unwrap()
                        .written_ahead
                        .contains_key(&offset),
                    "replacement is still write-ahead"
                );
            },
        );
        assert_old_pad_not_packed(wrote, &module, "durable point before push, lane down");
        Ok(())
    }

    /// Same gap, with the lane thread committing the replacement.
    #[test]
    fn cached_pad_is_not_packed_after_a_durable_point_on_the_lane() -> eyre::Result<()> {
        let (_tmp, module, chunk) = cache_module("cache_durable_lane")?;
        cache_durable_entropy(&module, 0x11)?;
        let module = Arc::new(module);
        let lane = StopLane::start(Arc::clone(&module))?;
        let offset = PartitionChunkOffset::from(0);
        let wrote = ingress_during_pause(
            &module,
            &chunk,
            super::EntropyPackPause::AfterEntropyCopied,
            || {
                assert!(module.write_chunk(
                    offset,
                    vec![0x22; 32],
                    super::super::ChunkType::Entropy
                ));
                module
                    .sync_pending_chunks_inner(true)
                    .expect("lane replacement sync");
                assert!(
                    pending_head(&module).is_none(),
                    "replacement is still pending"
                );
            },
        );
        assert_old_pad_not_packed(wrote, &module, "durable point before push, lane up");
        drop(lane);
        Ok(())
    }

    /// The fold already removed the slot and the pre-XOR check passed. A
    /// durable point on this thread (no lane) then clears both maps.
    #[test]
    fn cached_pad_is_not_packed_when_a_durable_point_lands_during_xor() -> eyre::Result<()> {
        let (_tmp, module, chunk) = cache_module("cache_xor_down")?;
        cache_durable_entropy(&module, 0x11)?;
        let module = Arc::new(module);
        let offset = PartitionChunkOffset::from(0);
        let wrote = ingress_during_pause(
            &module,
            &chunk,
            super::EntropyPackPause::BeforePackXor,
            || {
                assert!(module.write_chunk(
                    offset,
                    vec![0x22; 32],
                    super::super::ChunkType::Entropy
                ));
                module
                    .sync_pending_chunks_inner(true)
                    .expect("replacement sync during xor");
                assert!(
                    pending_head(&module).is_none(),
                    "replacement is still pending"
                );
                assert!(
                    !module
                        .pending_writes
                        .read()
                        .unwrap()
                        .written_ahead
                        .contains_key(&offset)
                );
            },
        );
        assert_old_pad_not_packed(wrote, &module, "durable point during xor, lane down");
        Ok(())
    }

    /// No later entropy write. The cached pad packs.
    #[test]
    fn fresh_cached_entropy_still_packs() -> eyre::Result<()> {
        let (_tmp, module, chunk) = cache_module("cache_fresh")?;
        cache_durable_entropy(&module, 0x11)?;
        let hits = module.disk.entropy_readahead_hits.load(Ordering::SeqCst);
        module.write_data_chunk(&chunk)?;
        assert!(
            module.disk.entropy_readahead_hits.load(Ordering::SeqCst) > hits,
            "the pack did not use the cached pad"
        );
        assert_fresh_pad_packed(&module);
        Ok(())
    }

    /// The lane packs the same fresh pad.
    #[test]
    fn fresh_cached_entropy_still_packs_on_the_lane() -> eyre::Result<()> {
        let (_tmp, module, chunk) = cache_module("cache_fresh_lane")?;
        cache_durable_entropy(&module, 0x11)?;
        let module = Arc::new(module);
        let lane = StopLane::start(Arc::clone(&module))?;
        let hits = module.disk.entropy_readahead_hits.load(Ordering::SeqCst);
        module.write_data_chunk(&chunk)?;
        assert!(
            module.disk.entropy_readahead_hits.load(Ordering::SeqCst) > hits,
            "the lane pack did not use the cached pad"
        );
        assert_fresh_pad_packed(&module);
        drop(lane);
        Ok(())
    }

    fn queue_entropy(module: &super::super::StorageModule, byte: u8) {
        assert!(
            module.write_chunk(
                PartitionChunkOffset::from(0),
                vec![byte; 32],
                super::super::ChunkType::Entropy
            ),
            "entropy enqueue failed"
        );
    }

    /// Replace the copied entropy. `sync` finishes the durable point on this
    /// thread, or on the lane when that thread is running.
    fn replace_entropy(module: &super::super::StorageModule, sync: bool) {
        queue_entropy(module, 0x22);
        if sync {
            module
                .sync_pending_chunks_inner(true)
                .expect("replacement sync");
            assert!(
                pending_head(module).is_none(),
                "replacement is still pending"
            );
        }
    }

    fn assert_newer_entropy_kept(module: &super::super::StorageModule) {
        assert_eq!(
            pending_head(module),
            Some((super::super::ChunkType::Entropy, Some(0x22))),
            "the newer entropy write is no longer pending"
        );
    }

    /// Occupy copied pending entropy `0x11`. A newer pending write replaces
    /// that map entry before the slot is queued. The lane is down.
    #[test]
    fn pending_entropy_is_not_packed_over_a_newer_write() -> eyre::Result<()> {
        let (_tmp, module, chunk) = cache_module("pending_newer")?;
        queue_entropy(&module, 0x11);
        let module = Arc::new(module);
        let wrote = ingress_during_pause(
            &module,
            &chunk,
            super::EntropyPackPause::AfterEntropyCopied,
            || replace_entropy(&module, false),
        );
        assert_old_pad_not_packed(wrote, &module, "newer pending entropy before push");
        assert_newer_entropy_kept(&module);
        Ok(())
    }

    /// The fold already accepted the clone. The newer bytes land during the
    /// XOR, while the lane is down, and stay pending.
    #[test]
    fn pending_entropy_is_not_packed_when_a_newer_write_lands_during_xor() -> eyre::Result<()> {
        let (_tmp, module, chunk) = cache_module("pending_xor")?;
        queue_entropy(&module, 0x11);
        let module = Arc::new(module);
        let wrote = ingress_during_pause(
            &module,
            &chunk,
            super::EntropyPackPause::BeforePackXor,
            || replace_entropy(&module, false),
        );
        assert_old_pad_not_packed(wrote, &module, "newer pending entropy during xor");
        assert_newer_entropy_kept(&module);
        Ok(())
    }

    /// Same gap as the queued-body case. The replacement reaches a durable
    /// point, so both maps are empty. The lane is down.
    #[test]
    fn pending_entropy_is_not_packed_after_a_durable_point_without_a_lane() -> eyre::Result<()> {
        let (_tmp, module, chunk) = cache_module("pending_durable_down")?;
        queue_entropy(&module, 0x11);
        let module = Arc::new(module);
        let wrote = ingress_during_pause(
            &module,
            &chunk,
            super::EntropyPackPause::AfterEntropyCopied,
            || replace_entropy(&module, true),
        );
        assert_old_pad_not_packed(wrote, &module, "durable point before push, lane down");
        assert!(pending_head(&module).is_none());
        Ok(())
    }

    /// The pre-XOR check passed on the original bytes. A durable point then
    /// clears them. No lane is running, so this thread syncs during the XOR.
    #[test]
    fn pending_entropy_is_not_packed_when_a_durable_point_lands_during_xor() -> eyre::Result<()> {
        let (_tmp, module, chunk) = cache_module("pending_xor_durable")?;
        queue_entropy(&module, 0x11);
        let module = Arc::new(module);
        let wrote = ingress_during_pause(
            &module,
            &chunk,
            super::EntropyPackPause::BeforePackXor,
            || replace_entropy(&module, true),
        );
        assert_old_pad_not_packed(wrote, &module, "durable point during xor, lane down");
        assert!(pending_head(&module).is_none());
        Ok(())
    }

    /// The lane commits the replacement while the clone is still off the queue.
    #[test]
    fn pending_entropy_is_not_packed_after_a_durable_point_on_the_lane() -> eyre::Result<()> {
        let (_tmp, module, chunk) = cache_module("pending_durable_lane")?;
        queue_entropy(&module, 0x11);
        let module = Arc::new(module);
        let lane = StopLane::start(Arc::clone(&module))?;
        let wrote = ingress_during_pause(
            &module,
            &chunk,
            super::EntropyPackPause::AfterEntropyCopied,
            || replace_entropy(&module, true),
        );
        assert_old_pad_not_packed(wrote, &module, "durable point before push, lane up");
        assert!(pending_head(&module).is_none());
        drop(lane);
        Ok(())
    }

    /// The lane is up and the replacement stays unsynced until the lane pass.
    /// That pass may durable-point the new bytes before the fold, or the fold
    /// may see them still pending. Neither order may pack the old clone.
    #[test]
    fn pending_entropy_is_not_packed_over_a_newer_write_on_the_lane() -> eyre::Result<()> {
        let (_tmp, module, chunk) = cache_module("pending_newer_lane")?;
        queue_entropy(&module, 0x11);
        let module = Arc::new(module);
        let lane = StopLane::start(Arc::clone(&module))?;
        let wrote = ingress_during_pause(
            &module,
            &chunk,
            super::EntropyPackPause::AfterEntropyCopied,
            || replace_entropy(&module, false),
        );
        assert_old_pad_not_packed(wrote, &module, "newer pending entropy, lane up");
        let head = pending_head(&module);
        assert_ne!(
            head,
            Some((super::super::ChunkType::Data, Some(0x10))),
            "packed the entropy copied before the replacement"
        );
        drop(lane);
        Ok(())
    }

    /// Pending entropy with no later write still packs. The interval may not
    /// be Entropy yet: the bytes are only queued.
    #[test]
    fn fresh_pending_entropy_still_packs() -> eyre::Result<()> {
        let (_tmp, module, chunk) = cache_module("pending_fresh")?;
        queue_entropy(&module, 0x11);
        module.write_data_chunk(&chunk)?;
        assert_fresh_pad_packed(&module);
        Ok(())
    }

    /// The lane may durable-point that entropy before the fold. The first
    /// call then returns backpressure and the retry reads the new bytes.
    #[test]
    fn fresh_pending_entropy_still_packs_on_the_lane() -> eyre::Result<()> {
        let (_tmp, module, chunk) = cache_module("pending_fresh_lane")?;
        let module = Arc::new(module);
        let lane = StopLane::start(Arc::clone(&module))?;
        queue_entropy(&module, 0x11);
        let first = module.write_data_chunk(&chunk);
        if matches!(first, Err(super::super::WriteDataChunkError::Backpressure)) {
            module.write_data_chunk(&chunk)?;
        } else {
            first?;
        }
        assert_fresh_pad_packed(&module);
        drop(lane);
        Ok(())
    }

    fn overwrite_chunk_file(
        module: &super::super::StorageModule,
        offset: PartitionChunkOffset,
        byte: u8,
    ) -> eyre::Result<()> {
        use std::os::unix::fs::FileExt as _;
        let len = module.config.consensus.chunk_size.max(1) as usize;
        let (file, at, _) = module.locate_io(offset, true)?;
        let file = module.disk.lock_chunks(&file);
        file.write_all_at(&vec![byte; len], at)?;
        Ok(())
    }

    /// The epoch moved and the cached bytes are still the bytes on disk.
    /// A write at another offset bumps the same counter. This pad still packs.
    #[test]
    fn cached_pad_still_packs_when_only_the_epoch_changes() -> eyre::Result<()> {
        let (_tmp, module, chunk) = cache_module("cache_epoch_same_bytes")?;
        cache_durable_entropy(&module, 0x11)?;
        let hits = module.disk.entropy_readahead_hits.load(Ordering::SeqCst);
        module.entropy_write_epoch.fetch_add(1, Ordering::SeqCst);
        assert_eq!(module.entropy_cache_offsets_for_test(), vec![0]);
        module.write_data_chunk(&chunk)?;
        assert_fresh_pad_packed(&module);
        assert!(
            module.disk.entropy_readahead_hits.load(Ordering::SeqCst) > hits,
            "a packed cache pad was not counted"
        );
        Ok(())
    }

    /// Entropy at offset 1 reaches a durable point. The pad at offset 0 is
    /// still the file bytes and must pack. The hit is counted at the pack.
    #[test]
    fn unrelated_entropy_write_still_packs_the_cached_pad() -> eyre::Result<()> {
        let (_tmp, module, chunk) = cache_module("cache_unrelated")?;
        cache_durable_entropy(&module, 0x11)?;
        let epoch = module.entropy_write_epoch.load(Ordering::SeqCst);
        let hits = module.disk.entropy_readahead_hits.load(Ordering::SeqCst);
        assert!(module.write_chunk(
            PartitionChunkOffset::from(1),
            vec![0x22; 32],
            super::super::ChunkType::Entropy
        ));
        module.sync_pending_chunks_inner(true)?;
        assert!(module.entropy_write_epoch.load(Ordering::SeqCst) > epoch);
        assert_eq!(
            module.entropy_cache_offsets_for_test(),
            vec![0],
            "the other offset's write dropped this pad"
        );
        module.write_data_chunk(&chunk)?;
        assert_fresh_pad_packed(&module);
        assert!(
            module.disk.entropy_readahead_hits.load(Ordering::SeqCst) > hits,
            "a packed cache pad was not counted"
        );
        Ok(())
    }

    /// The pre-XOR check passed. An entropy durable point at another offset
    /// lands during the XOR. The pad bytes are still on disk and must pack.
    #[test]
    fn unrelated_entropy_write_during_xor_still_packs_the_cached_pad() -> eyre::Result<()> {
        let (_tmp, module, chunk) = cache_module("cache_xor_unrelated")?;
        cache_durable_entropy(&module, 0x11)?;
        let module = Arc::new(module);
        let wrote = ingress_during_pause(
            &module,
            &chunk,
            super::EntropyPackPause::BeforePackXor,
            || {
                assert!(module.write_chunk(
                    PartitionChunkOffset::from(1),
                    vec![0x22; 32],
                    super::super::ChunkType::Entropy
                ));
                module
                    .sync_pending_chunks_inner(true)
                    .expect("unrelated sync during xor");
            },
        );
        wrote.expect("unrelated epoch move during xor rejected the pad");
        assert_fresh_pad_packed(&module);
        Ok(())
    }

    /// One lane pass pwrites the unrelated entropy, then folds the pad.
    /// The fold runs after that bump and must still pack the pad.
    #[test]
    fn lane_pass_packs_a_cached_pad_after_an_unrelated_entropy_write() -> eyre::Result<()> {
        let (_tmp, module, chunk) = cache_module("cache_lane_pass")?;
        cache_durable_entropy(&module, 0x11)?;
        let epoch = module.entropy_write_epoch.load(Ordering::SeqCst);
        assert!(module.write_chunk(
            PartitionChunkOffset::from(1),
            vec![0x22; 32],
            super::super::ChunkType::Entropy
        ));
        let queued = module.enqueue_ingress_batch(std::slice::from_ref(&chunk))?;
        assert!(
            matches!(
                queued.first(),
                Some(super::super::BatchEnqueueItem::Queued(_))
            ),
            "{queued:?}"
        );
        module.disk_lane_pass();
        assert!(
            module.entropy_write_epoch.load(Ordering::SeqCst) > epoch,
            "the lane pass did not durable-point the unrelated entropy"
        );
        assert_fresh_pad_packed(&module);
        assert_eq!(module.sweep_slot_count_for_test(), 0);
        Ok(())
    }

    /// The sweep already read offset 0. A durable point at offset 1 bumps
    /// the epoch before the XOR. Offset 0's bytes did not change.
    #[test]
    fn pread_packs_when_another_offset_is_rewritten() -> eyre::Result<()> {
        let (_tmp, module) = span_fixture("pread_unrelated", 4, 4, 0)?;
        let offset = PartitionChunkOffset::from(0);
        assert!(module.write_chunk(offset, vec![0x11; 32], super::super::ChunkType::Entropy));
        module.sync_pending_chunks_inner(true)?;
        module.pause_next_sync_at(super::super::SyncPausePoint::AfterEntropyPread);
        let mut slot = sweep_slot(0);
        slot.unpacked = Arc::new(vec![0x01; 32]);
        slot.byte_len = 32;
        let mut rx = arm_slot_group(&module, slot);
        std::thread::scope(|scope| {
            let sweep = scope.spawn(|| module.sweep_one());
            assert!(
                module.wait_for_sync_pause(Duration::from_secs(5)),
                "sweep did not pause after the entropy pread"
            );
            assert!(module.write_chunk(
                PartitionChunkOffset::from(1),
                vec![0x22; 32],
                super::super::ChunkType::Entropy
            ));
            module
                .sync_pending_chunks_inner(true)
                .expect("unrelated sync");
            module.release_sync_pause();
            assert!(sweep.join().expect("sweep thread"), "sweep did not read");
        });
        rx.try_recv()
            .expect("slot finished")
            .expect("unrelated epoch move rejected the pread");
        let pending = module.pending_writes.read().unwrap();
        let (bytes, kind) = pending.get(&offset).expect("packed data");
        assert_eq!(*kind, super::super::ChunkType::Data);
        assert_eq!(bytes.first().copied(), Some(0x10));
        Ok(())
    }

    /// The batch caller has already returned. A same-offset rewrite must not
    /// drop the body. The slot is read again and packs the new entropy.
    #[test]
    fn ingress_batch_rereads_a_pad_after_the_same_offset_is_rewritten() -> eyre::Result<()> {
        let (_tmp, module, chunk) = cache_module("batch_reread")?;
        cache_durable_entropy(&module, 0x11)?;
        let hits = module.disk.entropy_readahead_hits.load(Ordering::SeqCst);
        let queued = module.enqueue_ingress_batch(std::slice::from_ref(&chunk))?;
        assert!(
            matches!(
                queued.first(),
                Some(super::super::BatchEnqueueItem::Queued(_))
            ),
            "{queued:?}"
        );
        let offset = PartitionChunkOffset::from(0);
        assert!(module.write_chunk(offset, vec![0x22; 32], super::super::ChunkType::Entropy));
        module.sync_pending_chunks_inner(true)?;
        assert_eq!(
            module.sweep_slot_count_for_test(),
            1,
            "the rejected pad was dropped instead of queued for a read"
        );
        module.drive_entropy_queue_for_test();
        assert_eq!(
            pending_head(&module),
            Some((super::super::ChunkType::Data, Some(0x23))),
            "the reread did not pack the replacement entropy"
        );
        assert_eq!(module.sweep_slot_count_for_test(), 0);
        assert_eq!(
            module.disk.entropy_readahead_hits.load(Ordering::SeqCst),
            hits,
            "a pad that was read from disk was counted as a cache hit"
        );
        Ok(())
    }

    /// A disk slot waiting on a newer pending write must not hold the drain
    /// open. The write lands on a later pass, and this call returns.
    #[test]
    fn drain_returns_while_a_reread_waits_on_pending_entropy() -> eyre::Result<()> {
        let (_tmp, module, chunk) = cache_module("drain_blocked")?;
        cache_durable_entropy(&module, 0x11)?;
        let queued = module.enqueue_ingress_batch(std::slice::from_ref(&chunk))?;
        assert!(matches!(
            queued.first(),
            Some(super::super::BatchEnqueueItem::Queued(_))
        ));
        assert!(module.write_chunk(
            PartitionChunkOffset::from(0),
            vec![0x22; 32],
            super::super::ChunkType::Entropy
        ));
        assert_eq!(module.sweep_slot_count_for_test(), 1);
        let started = Instant::now();
        module.drive_entropy_queue_for_test();
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "drain waited {elapsed:?} on a slot blocked by pending entropy"
        );
        assert_eq!(
            module.sweep_slot_count_for_test(),
            1,
            "the blocked reread was dropped"
        );
        assert_ne!(
            pending_head(&module),
            Some((super::super::ChunkType::Data, Some(0x10))),
            "packed the pad while the replacement entropy was still pending"
        );
        Ok(())
    }

    /// A young span is not eligible, so the drain waits. Notifies during that
    /// wait must not push the give-up out. The span stays queued.
    #[test]
    fn drain_returns_by_the_give_up_while_notifies_keep_waking_it() -> eyre::Result<()> {
        let grace = Duration::from_millis(1_500);
        let give_up = Duration::from_millis(200);
        let (_tmp, module) =
            span_fixture_grace("drain_notify_give_up", 8, 8, 0, grace.as_millis() as u64, 0)?;
        module.disk.set_sweep_wait_for_test(give_up);
        // A wake after `seen` was sampled and before the deadline check.
        // A sleeping notifier loses that race and the give-up still runs.
        module.disk.set_notify_on_wait_for_test(true);
        insert_slots(&module, &[0, 1], Instant::now());
        let module = Arc::new(module);
        let (tx, rx) = mpsc::channel();
        let worker = {
            let module = Arc::clone(&module);
            thread::spawn(move || {
                let started = Instant::now();
                module.drive_entropy_queue_for_test();
                let _ = tx.send(started.elapsed());
            })
        };
        let result = rx.recv_timeout(Duration::from_millis(1_000));
        let elapsed = match result {
            Ok(elapsed) => elapsed,
            Err(timeout) => {
                module.disk.set_notify_on_wait_for_test(false);
                let late = rx.recv_timeout(grace + Duration::from_secs(2));
                let _ = worker.join();
                panic!("drain did not return under notifies: {timeout} late={late:?}");
            }
        };
        module.disk.set_notify_on_wait_for_test(false);
        worker.join().expect("drain thread");
        assert!(
            elapsed >= give_up,
            "drain returned in {elapsed:?}, before the give-up"
        );
        assert!(
            elapsed < Duration::from_millis(800),
            "drain ran {elapsed:?} while notifies kept waking it"
        );
        assert_eq!(
            module.sweep_slot_count_for_test(),
            2,
            "the give-up dropped the span"
        );
        module.fail_queued_sweeps();
        Ok(())
    }

    /// A notify before the give-up is not a reason to stop. The span is still
    /// swept once the read grace ends.
    #[test]
    fn drain_still_sweeps_when_a_notify_arrives_before_the_give_up() -> eyre::Result<()> {
        let grace = Duration::from_millis(150);
        let (_tmp, module) = span_fixture_grace(
            "drain_notify_continues",
            8,
            8,
            0,
            grace.as_millis() as u64,
            0,
        )?;
        insert_slots(&module, &[0, 1], Instant::now());
        let module = Arc::new(module);
        let stop = Arc::new(AtomicBool::new(false));
        let notifier = {
            let module = Arc::clone(&module);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    module.disk.notify();
                    thread::sleep(Duration::from_millis(5));
                }
            })
        };
        let started = Instant::now();
        module.drive_entropy_queue_for_test();
        let elapsed = started.elapsed();
        stop.store(true, Ordering::SeqCst);
        notifier.join().expect("notifier");
        assert!(
            elapsed >= Duration::from_millis(100),
            "drain returned in {elapsed:?}, before the span was eligible"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "a notify before the give-up ended the drain: {elapsed:?}"
        );
        assert_eq!(
            module.sweep_slot_count_for_test(),
            0,
            "the notify ended the drain before the sweep"
        );
        assert!(
            module.disk.entropy_preads.load(Ordering::SeqCst) >= 1,
            "the span was dropped without a pread"
        );
        Ok(())
    }

    /// A recall that stays up is the same give-up. The drain must not block
    /// in `yield_to_recall` until the recall drops.
    #[test]
    fn drain_returns_when_a_recall_holds_past_the_give_up() -> eyre::Result<()> {
        let give_up = Duration::from_millis(200);
        let (_tmp, module) = span_fixture_grace("drain_recall_give_up", 8, 8, 0, 0, 0)?;
        module.disk.set_sweep_wait_for_test(give_up);
        {
            let mut queue = module.disk.queue();
            let mut slot = sweep_slot(0);
            slot.pending_entropy = Some(vec![0x11; 32]);
            queue.insert_slot(slot);
        }
        let module = Arc::new(module);
        let hold = module.disk.begin_recall();
        let (tx, rx) = mpsc::channel();
        let worker = {
            let module = Arc::clone(&module);
            thread::spawn(move || {
                let started = Instant::now();
                module.drive_entropy_queue_for_test();
                let _ = tx.send(started.elapsed());
            })
        };
        let result = rx.recv_timeout(Duration::from_millis(1_000));
        drop(hold);
        let elapsed = match result {
            Ok(elapsed) => elapsed,
            Err(timeout) => {
                let late = rx.recv_timeout(Duration::from_secs(3));
                let _ = worker.join();
                panic!(
                    "drain did not return while a recall held the disk: {timeout} late={late:?}"
                );
            }
        };
        worker.join().expect("drain thread");
        assert!(
            elapsed >= give_up,
            "drain returned in {elapsed:?}, before the give-up"
        );
        assert!(
            elapsed < Duration::from_millis(800),
            "drain waited {elapsed:?} while a recall held the disk"
        );
        assert_eq!(
            module.sweep_slot_count_for_test(),
            1,
            "the drain packed while the recall still held the disk"
        );
        module.fail_queued_sweeps();
        Ok(())
    }

    /// A reread waiting on a queued write is not due. Another span still is.
    /// After that write leaves the map, the waiting span is due again.
    #[test]
    fn blocked_reread_does_not_hide_another_span() -> eyre::Result<()> {
        let (_tmp, module) = span_fixture("blocked_ready", 8, 1, 0)?;
        insert_slots(&module, &[0, 1], Instant::now());
        assert_eq!(module.read_grace_remaining(), Some(Duration::ZERO));
        {
            let mut pending = module.pending_writes.write().unwrap();
            pending.insert(
                PartitionChunkOffset::from(0),
                (vec![0x22; 32], super::super::ChunkType::Entropy),
            );
        }
        assert!(
            module.has_ready_disk_span(),
            "a blocked offset hid the other span"
        );
        assert_eq!(taken_offsets(&module).expect("the unblocked span"), vec![1]);
        assert_eq!(module.sweep_slot_count_for_test(), 1);
        assert!(
            module.read_grace_remaining().is_none(),
            "the blocked span is still due"
        );
        assert!(
            !module.has_ready_disk_span(),
            "the blocked span kept the lane ready"
        );
        module
            .pending_writes
            .write()
            .unwrap()
            .remove(&PartitionChunkOffset::from(0));
        assert!(module.has_ready_disk_span());
        assert_eq!(module.read_grace_remaining(), Some(Duration::ZERO));
        assert_eq!(taken_offsets(&module).expect("the write landed"), vec![0]);
        Ok(())
    }

    fn ingress_stage_names(module: &super::super::StorageModule) -> Vec<&'static str> {
        module
            .disk
            .ingest_stages
            .lock()
            .unwrap()
            .iter()
            .map(|(stage, _)| *stage)
            .collect()
    }

    fn assert_memory_pack_staged(module: &super::super::StorageModule) {
        let stages = ingress_stage_names(module);
        assert!(
            stages.contains(&"entropy_to_packed"),
            "memory pack did not record entropy_to_packed: {stages:?}"
        );
        assert!(
            !stages.contains(&"queue_to_entropy"),
            "memory pack recorded queue_to_entropy: {stages:?}"
        );
    }

    /// A cache hit does the XOR and the packed insert. It did not pread.
    #[test]
    fn cached_pack_records_entropy_to_packed_only() -> eyre::Result<()> {
        let (_tmp, module, chunk) = cache_module("cache_stage")?;
        cache_durable_entropy(&module, 0x11)?;
        module.write_data_chunk(&chunk)?;
        assert_fresh_pad_packed(&module);
        assert_memory_pack_staged(&module);
        Ok(())
    }

    /// Pending entropy takes the same fold. It did not pread.
    #[test]
    fn pending_pack_records_entropy_to_packed_only() -> eyre::Result<()> {
        let (_tmp, module, chunk) = cache_module("pending_stage")?;
        queue_entropy(&module, 0x11);
        module.write_data_chunk(&chunk)?;
        assert_fresh_pad_packed(&module);
        assert_memory_pack_staged(&module);
        Ok(())
    }

    /// Each inflight span's queue stage ends when that pread returns.
    /// A stamp taken before `read_exact_at` returns fails. The join, and
    /// the XOR of an earlier span, are not part of it.
    #[test]
    fn each_inflight_span_stamps_its_pread_return() -> eyre::Result<()> {
        let (_tmp, module) = span_fixture("span_stamp", 8, 1, 0)?;
        assert!(module.write_chunk(
            PartitionChunkOffset::from(0),
            vec![0x11; 32],
            super::super::ChunkType::Entropy
        ));
        assert!(module.write_chunk(
            PartitionChunkOffset::from(2),
            vec![0x11; 32],
            super::super::ChunkType::Entropy
        ));
        module.sync_pending_chunks_inner(true)?;
        insert_slots(&module, &[0, 2], Instant::now());
        module.disk.inflight_pread_hold.arm();
        std::thread::scope(|scope| {
            let sweep = scope.spawn(|| module.sweep_inflight());
            assert!(
                module
                    .disk
                    .inflight_pread_hold
                    .wait_arrived(2, Duration::from_secs(5)),
                "the inflight preads did not both return"
            );
            let release_at = Instant::now();
            module.disk.inflight_pread_hold.release();
            sweep.join().expect("inflight sweep");
            let stamps = module.disk.span_read_done.lock().unwrap().clone();
            let returned = module.disk.span_pread_returned.lock().unwrap().clone();
            assert_eq!(stamps.len(), 2, "{stamps:?}");
            for (offset, at) in stamps {
                let pread_returned = returned
                    .iter()
                    .find(|(start_offset, _)| *start_offset == offset)
                    .map(|(_, returned_at)| *returned_at)
                    .unwrap_or_else(|| panic!("offset {offset} has no pread return"));
                assert!(
                    at >= pread_returned,
                    "offset {offset} was stamped at {at:?}, before read_exact_at returned at {pread_returned:?}"
                );
                assert!(
                    at < release_at,
                    "offset {offset} was stamped at {at:?}, after the preads were released at {release_at:?}"
                );
            }
        });
        Ok(())
    }

    fn chunk_file_byte(
        module: &super::super::StorageModule,
        offset: PartitionChunkOffset,
    ) -> eyre::Result<u8> {
        use std::os::unix::fs::FileExt as _;
        let (file, at, _) = module.locate_io(offset, true)?;
        let file = module.disk.lock_chunks(&file);
        let mut buf = [0_u8; 1];
        file.read_exact_at(&mut buf, at)?;
        Ok(buf[0])
    }

    /// Cached pad `0x11`, then an ingress body `0x01`, then entropy `0x22`
    /// at the same offset. The batch has no waiter, so the slot stays queued
    /// for a disk read and that read waits on the new entropy.
    fn queue_blocked_reread(
        prefix: &str,
    ) -> eyre::Result<(
        irys_testing_utils::utils::tempfile::TempDir,
        super::super::StorageModule,
        UnpackedChunk,
    )> {
        let (tmp, module, chunk) = cache_module(prefix)?;
        cache_durable_entropy(&module, 0x11)?;
        let queued = module.enqueue_ingress_batch(std::slice::from_ref(&chunk))?;
        assert!(
            matches!(
                queued.first(),
                Some(super::super::BatchEnqueueItem::Queued(_))
            ),
            "{queued:?}"
        );
        assert!(module.write_chunk(
            PartitionChunkOffset::from(0),
            vec![0x22; 32],
            super::super::ChunkType::Entropy
        ));
        assert_eq!(module.sweep_slot_count_for_test(), 1);
        Ok((tmp, module, chunk))
    }

    fn assert_replacement_body_durable(module: &super::super::StorageModule) -> eyre::Result<()> {
        let offset = PartitionChunkOffset::from(0);
        let durable = module.is_data_chunk_durable_at(offset);
        let persisted = module.is_data_chunk_persisted_at(offset);
        let slots = module.sweep_slot_count_for_test();
        let pending = pending_head(module);
        let ahead = module
            .pending_writes
            .read()
            .unwrap()
            .written_ahead
            .contains_key(&offset);
        let byte = chunk_file_byte(module, offset)?;
        assert!(
            durable && persisted && slots == 0 && pending.is_none() && !ahead && byte == 0x23,
            "flush durable={durable} persisted={persisted} slots={slots} pending={pending:?} ahead={ahead} file={byte:#x}"
        );
        Ok(())
    }

    /// The blocking entropy is still in `pending.chunks`. `force_sync` must
    /// commit it, then read and flush the body, before it returns.
    #[test]
    fn force_sync_packs_a_reread_blocked_on_pending_entropy() -> eyre::Result<()> {
        let (_tmp, module, _chunk) = queue_blocked_reread("force_blocked_pending")?;
        module.force_sync_pending_chunks()?;
        assert_replacement_body_durable(&module)
    }

    /// Same body, after the blocking entropy has been pwritten and parked.
    /// The reread stays queued until that durable point.
    #[test]
    fn force_sync_packs_a_reread_blocked_on_written_ahead() -> eyre::Result<()> {
        let (tmp, module) = span_fixture_timed("force_blocked_ahead", 4, 4, 0, 0, 32 * 4, 60_000)?;
        let chunk = index_unpacked_byte(&module)?;
        cache_durable_entropy(&module, 0x11)?;
        let queued = module.enqueue_ingress_batch(std::slice::from_ref(&chunk))?;
        assert!(
            matches!(
                queued.first(),
                Some(super::super::BatchEnqueueItem::Queued(_))
            ),
            "{queued:?}"
        );
        let offset = PartitionChunkOffset::from(0);
        assert!(module.write_chunk(offset, vec![0x22; 32], super::super::ChunkType::Entropy));
        module.commit_pending_runs(false, Some(1), false)?;
        assert!(
            module
                .pending_writes
                .read()
                .unwrap()
                .written_ahead
                .contains_key(&offset),
            "the blocking entropy was not parked"
        );
        assert!(
            pending_head(&module).is_none(),
            "parked entropy is still in pending.chunks"
        );
        assert_eq!(module.sweep_slot_count_for_test(), 1);
        module.force_sync_pending_chunks()?;
        assert_replacement_body_durable(&module)?;
        drop(module);
        drop(tmp);
        Ok(())
    }

    /// Clears a sweep queue the test left behind. `Drop` drains that queue,
    /// and a blocked disk slot beside a lower memory slot would wait out
    /// `SWEEP_WAIT` before the assertion failure is reported.
    struct FailSweepOnDrop<'a> {
        module: &'a super::super::StorageModule,
        armed: bool,
    }

    impl Drop for FailSweepOnDrop<'_> {
        fn drop(&mut self) {
            if self.armed {
                self.module.fail_queued_sweeps();
            }
        }
    }

    fn indexed_chunk(
        module: &super::super::StorageModule,
        ledger_offset: u64,
        root_byte: u8,
        body: u8,
    ) -> eyre::Result<UnpackedChunk> {
        let data_root = H256::repeat_byte(root_byte);
        let path = vec![root_byte, 3, 2, 1];
        let header = DataTransactionHeader::V1(irys_types::DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                data_root,
                data_size: 32,
                ..Default::default()
            },
            metadata: irys_types::DataTransactionMetadata::new(),
        });
        module.index_transaction_data(
            &header,
            &path,
            LedgerChunkRange(ledger_chunk_offset_ii!(ledger_offset, ledger_offset)),
            0,
        )?;
        Ok(UnpackedChunk {
            data_root,
            data_size: 32,
            data_path: path.into(),
            bytes: vec![body; 32].into(),
            tx_offset: TxChunkOffset::from(0),
        })
    }

    fn assert_slot_shape(
        module: &super::super::StorageModule,
        offset: u32,
        priority: super::WritePriority,
        memory: bool,
    ) {
        let queue = module.disk.queue();
        let slot = queue
            .slots
            .get(&PartitionChunkOffset::from(offset))
            .unwrap_or_else(|| panic!("offset {offset} is not queued"));
        assert_eq!(slot.priority, priority, "offset {offset}");
        assert_eq!(
            slot.pending_entropy.is_some() || slot.cached_entropy.is_some(),
            memory,
            "offset {offset} memory={memory} pending={:?} cached={}",
            slot.pending_entropy
                .as_ref()
                .map(|bytes| bytes.first().copied()),
            slot.cached_entropy.is_some()
        );
    }

    fn assert_data_byte_durable(
        module: &super::super::StorageModule,
        offset: u32,
        byte: u8,
    ) -> eyre::Result<()> {
        let offset = PartitionChunkOffset::from(offset);
        let durable = module.is_data_chunk_durable_at(offset);
        let persisted = module.is_data_chunk_persisted_at(offset);
        let pending = module.pending_writes.read().unwrap();
        let queued = pending
            .get(&offset)
            .map(|(bytes, kind)| (*kind, bytes.first().copied()));
        let ahead = pending.written_ahead.contains_key(&offset);
        drop(pending);
        let file = chunk_file_byte(module, offset)?;
        assert!(
            durable && persisted && queued.is_none() && !ahead && file == byte,
            "offset {offset} durable={durable} persisted={persisted} pending={queued:?} ahead={ahead} file={file:#x} want={byte:#x}"
        );
        Ok(())
    }

    /// Migration at offset 0 is a disk reread: its entropy is parked in
    /// `written_ahead` and the interval is already `Entropy`, so occupy does
    /// not copy a pad. Ingress at offset 1 still holds pending entropy.
    /// Migration outranks ingress, so the memory pad cannot fold.
    fn queue_written_ahead_reread_and_lower_memory(
        prefix: &str,
    ) -> eyre::Result<(
        irys_testing_utils::utils::tempfile::TempDir,
        super::super::StorageModule,
    )> {
        let (tmp, module) = span_fixture_timed(prefix, 4, 4, 0, 0, 0, 60_000)?;
        let migration = indexed_chunk(&module, 0, 0x42, 0x01)?;
        let ingress = indexed_chunk(&module, 1, 0x43, 0x05)?;
        // The first durable point always commits. A point just taken makes
        // the next non-force window park instead.
        *module.last_durable_at.lock().unwrap() = Some(Instant::now());
        assert!(module.write_chunk(
            PartitionChunkOffset::from(0),
            vec![0x22; 32],
            super::super::ChunkType::Entropy
        ));
        module.commit_pending_runs(false, Some(1), false)?;
        {
            let pending = module.pending_writes.read().unwrap();
            assert!(
                pending
                    .written_ahead
                    .contains_key(&PartitionChunkOffset::from(0)),
                "the blocking entropy was not parked"
            );
            assert!(
                pending.get(&PartitionChunkOffset::from(0)).is_none(),
                "parked entropy is still in pending.chunks"
            );
        }
        assert_eq!(
            module.get_chunk_type(&PartitionChunkOffset::from(0)),
            Some(super::super::ChunkType::Entropy),
            "the parked write did not label the interval"
        );
        assert!(module.write_chunk(
            PartitionChunkOffset::from(1),
            vec![0x11; 32],
            super::super::ChunkType::Entropy
        ));
        module.deposit_data_chunk(&migration)?;
        let queued = module.enqueue_ingress_batch(std::slice::from_ref(&ingress))?;
        assert!(
            matches!(
                queued.first(),
                Some(super::super::BatchEnqueueItem::Queued(_))
            ),
            "{queued:?}"
        );
        assert_eq!(module.sweep_slot_count_for_test(), 2);
        assert_slot_shape(&module, 0, super::WritePriority::Migration, false);
        assert_slot_shape(&module, 1, super::WritePriority::Ingress, true);
        Ok((tmp, module))
    }

    /// Same shape, with the blocking entropy still in `pending.chunks`.
    /// The migration slot is queued against durable entropy, then the rewrite
    /// drops its pad and leaves a disk reread.
    fn queue_pending_reread_and_lower_memory(
        prefix: &str,
    ) -> eyre::Result<(
        irys_testing_utils::utils::tempfile::TempDir,
        super::super::StorageModule,
    )> {
        let (tmp, module) = span_fixture_timed(prefix, 4, 4, 0, 0, 0, 60_000)?;
        let migration = indexed_chunk(&module, 0, 0x42, 0x01)?;
        let ingress = indexed_chunk(&module, 1, 0x43, 0x05)?;
        assert!(module.write_chunk(
            PartitionChunkOffset::from(0),
            vec![0x11; 32],
            super::super::ChunkType::Entropy
        ));
        module.sync_pending_chunks_inner(true)?;
        module.deposit_data_chunk(&migration)?;
        assert_eq!(module.sweep_slot_count_for_test(), 1);
        assert!(module.write_chunk(
            PartitionChunkOffset::from(0),
            vec![0x22; 32],
            super::super::ChunkType::Entropy
        ));
        assert!(
            module
                .pending_writes
                .read()
                .unwrap()
                .contains_key(&PartitionChunkOffset::from(0)),
            "the blocking entropy is not pending"
        );
        assert!(module.write_chunk(
            PartitionChunkOffset::from(1),
            vec![0x11; 32],
            super::super::ChunkType::Entropy
        ));
        let queued = module.enqueue_ingress_batch(std::slice::from_ref(&ingress))?;
        assert!(
            matches!(
                queued.first(),
                Some(super::super::BatchEnqueueItem::Queued(_))
            ),
            "{queued:?}"
        );
        assert_eq!(module.sweep_slot_count_for_test(), 2);
        assert_slot_shape(&module, 0, super::WritePriority::Migration, false);
        assert_slot_shape(&module, 1, super::WritePriority::Ingress, true);
        Ok((tmp, module))
    }

    fn assert_mixed_queue_cannot_pack(module: &super::super::StorageModule) {
        assert!(
            !module.memory_entropy_can_fold(),
            "the lower memory pad can fold while the disk reread is best"
        );
        assert!(
            module.read_grace_remaining().is_none(),
            "a blocked disk span is inside the read grace"
        );
        assert!(
            !module.fold_memory_entropy(),
            "fold packed a slot this queue cannot fold"
        );
        assert_eq!(module.sweep_slot_count_for_test(), 2);
        assert!(
            !module.sweep_one(),
            "sweep read a span that is waiting on a write"
        );
        assert_eq!(
            module.sweep_slot_count_for_test(),
            2,
            "sweep dropped a slot it could not read"
        );
    }

    /// The blocked disk reread and the lower memory pad stay queued. The
    /// drain must return so the force flush can commit the parked entropy.
    /// Both bodies are durable when that flush returns.
    #[test]
    fn mixed_written_ahead_reread_and_lower_memory_does_not_hold_the_drain() -> eyre::Result<()> {
        let (_tmp, module) = queue_written_ahead_reread_and_lower_memory("mixed_ahead_drain")?;
        let mut guard = FailSweepOnDrop {
            module: &module,
            armed: true,
        };
        assert_mixed_queue_cannot_pack(&module);
        let started = Instant::now();
        module.drive_entropy_queue_for_test();
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "drain waited {elapsed:?} on a blocked disk reread beside a lower memory pad"
        );
        assert_eq!(
            module.sweep_slot_count_for_test(),
            2,
            "the drain dropped slots the parked write still blocks"
        );
        assert!(
            module.only_pending_blocked_disk_slots(),
            "the mixed queue is not an early drain"
        );
        guard.armed = false;
        module.force_sync_pending_chunks()?;
        assert_eq!(module.sweep_slot_count_for_test(), 0);
        // 0x01 XOR parked entropy 0x22. 0x05 XOR pending entropy 0x11.
        assert_data_byte_durable(&module, 0, 0x23)?;
        assert_data_byte_durable(&module, 1, 0x14)?;
        Ok(())
    }

    /// The blocking bytes are still in `pending.chunks`. Same early return,
    /// then the force flush commits them and packs both bodies.
    #[test]
    fn mixed_pending_reread_and_lower_memory_does_not_hold_the_drain() -> eyre::Result<()> {
        let (_tmp, module) = queue_pending_reread_and_lower_memory("mixed_pending_drain")?;
        let mut guard = FailSweepOnDrop {
            module: &module,
            armed: true,
        };
        assert_mixed_queue_cannot_pack(&module);
        let started = Instant::now();
        module.drive_entropy_queue_for_test();
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "drain waited {elapsed:?} on a pending-blocked disk reread beside a lower memory pad"
        );
        assert_eq!(module.sweep_slot_count_for_test(), 2);
        assert!(module.only_pending_blocked_disk_slots());
        guard.armed = false;
        module.force_sync_pending_chunks()?;
        assert_eq!(module.sweep_slot_count_for_test(), 0);
        assert_data_byte_durable(&module, 0, 0x23)?;
        assert_data_byte_durable(&module, 1, 0x14)?;
        Ok(())
    }

    /// The disk span is not blocked. It is only young. The lower memory pad
    /// does not make this a blocked reread, so the drain waits out the grace
    /// and packs both slots.
    #[test]
    fn young_disk_span_still_waits_beside_lower_memory() -> eyre::Result<()> {
        let grace = Duration::from_millis(40);
        let (_tmp, module) = span_fixture_timed(
            "young_beside_memory",
            4,
            4,
            0,
            grace.as_millis() as u64,
            0,
            0,
        )?;
        let migration = indexed_chunk(&module, 0, 0x42, 0x01)?;
        let ingress = indexed_chunk(&module, 1, 0x43, 0x05)?;
        assert!(module.write_chunk(
            PartitionChunkOffset::from(0),
            vec![0x22; 32],
            super::super::ChunkType::Entropy
        ));
        module.sync_pending_chunks_inner(true)?;
        assert!(module.write_chunk(
            PartitionChunkOffset::from(1),
            vec![0x11; 32],
            super::super::ChunkType::Entropy
        ));
        module.deposit_data_chunk(&migration)?;
        let queued = module.enqueue_ingress_batch(std::slice::from_ref(&ingress))?;
        assert!(
            matches!(
                queued.first(),
                Some(super::super::BatchEnqueueItem::Queued(_))
            ),
            "{queued:?}"
        );
        assert_slot_shape(&module, 0, super::WritePriority::Migration, false);
        assert_slot_shape(&module, 1, super::WritePriority::Ingress, true);
        assert!(!module.memory_entropy_can_fold());
        assert!(!module.only_pending_blocked_disk_slots());
        let _guard = FailSweepOnDrop {
            module: &module,
            armed: true,
        };
        // Sample immediately before the drain. The floor is the grace still
        // left, not a later clock. A span that is already almost due fails
        // here instead of after a short wait.
        let remain = module
            .read_grace_remaining()
            .expect("the young disk span is a read wait");
        assert!(
            remain >= Duration::from_millis(20),
            "the young span is already almost due: {remain:?}"
        );
        let started = Instant::now();
        module.drive_entropy_queue_for_test();
        let elapsed = started.elapsed();
        // The sample is a few instructions before the drain reads the grace
        // again. The wait can be that much shorter than `remain`.
        let slack = Duration::from_millis(5);
        assert!(
            elapsed.saturating_add(slack) >= remain,
            "drain took {elapsed:?}, before the remaining grace {remain:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "drain waited {elapsed:?} on a span that became eligible"
        );
        assert_eq!(module.sweep_slot_count_for_test(), 0);
        assert_eq!(module.disk.entropy_preads.load(Ordering::SeqCst), 1);
        let pending = module.pending_writes.read().unwrap();
        let disk = pending
            .get(&PartitionChunkOffset::from(0))
            .map(|(bytes, kind)| (*kind, bytes.first().copied()));
        let memory = pending
            .get(&PartitionChunkOffset::from(1))
            .map(|(bytes, kind)| (*kind, bytes.first().copied()));
        assert_eq!(disk, Some((super::super::ChunkType::Data, Some(0x23))));
        assert_eq!(memory, Some((super::super::ChunkType::Data, Some(0x14))));
        Ok(())
    }

    /// One disk span can be read. Another is blocked, and a lower memory pad
    /// cannot fold. The drain reads the ready span and returns. It does not
    /// wait out the sweep give-up, and it does not drop the blocked reread.
    #[test]
    fn ready_span_drains_beside_a_blocked_slot_and_lower_memory() -> eyre::Result<()> {
        let (_tmp, module) = span_fixture_timed("ready_beside_blocked", 4, 4, 0, 0, 0, 60_000)?;
        let blocked = indexed_chunk(&module, 0, 0x42, 0x01)?;
        let ingress = indexed_chunk(&module, 1, 0x43, 0x05)?;
        let ready = indexed_chunk(&module, 2, 0x44, 0x07)?;
        assert!(module.write_chunk(
            PartitionChunkOffset::from(2),
            vec![0x33; 32],
            super::super::ChunkType::Entropy
        ));
        module.sync_pending_chunks_inner(true)?;
        assert!(module.write_chunk(
            PartitionChunkOffset::from(0),
            vec![0x22; 32],
            super::super::ChunkType::Entropy
        ));
        module.commit_pending_runs(false, Some(1), false)?;
        assert!(
            module
                .pending_writes
                .read()
                .unwrap()
                .written_ahead
                .contains_key(&PartitionChunkOffset::from(0)),
            "the blocked offset was not parked"
        );
        assert!(module.write_chunk(
            PartitionChunkOffset::from(1),
            vec![0x11; 32],
            super::super::ChunkType::Entropy
        ));
        module.deposit_data_chunks(&[blocked, ready])?;
        let queued = module.enqueue_ingress_batch(std::slice::from_ref(&ingress))?;
        assert!(
            matches!(
                queued.first(),
                Some(super::super::BatchEnqueueItem::Queued(_))
            ),
            "{queued:?}"
        );
        assert_eq!(module.sweep_slot_count_for_test(), 3);
        assert_slot_shape(&module, 0, super::WritePriority::Migration, false);
        assert_slot_shape(&module, 1, super::WritePriority::Ingress, true);
        assert_slot_shape(&module, 2, super::WritePriority::Migration, false);
        assert!(!module.only_pending_blocked_disk_slots());
        let mut guard = FailSweepOnDrop {
            module: &module,
            armed: true,
        };
        let started = Instant::now();
        module.drive_entropy_queue_for_test();
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "drain waited {elapsed:?} after the ready span was the only progress"
        );
        assert_eq!(module.disk.entropy_preads.load(Ordering::SeqCst), 1);
        let pending = module.pending_writes.read().unwrap();
        let packed = pending
            .get(&PartitionChunkOffset::from(2))
            .map(|(bytes, kind)| (*kind, bytes.first().copied()));
        assert_eq!(
            packed,
            Some((super::super::ChunkType::Data, Some(0x34))),
            "the ready span was not packed: {packed:?}"
        );
        drop(pending);
        assert_slot_shape(&module, 0, super::WritePriority::Migration, false);
        assert_slot_shape(&module, 1, super::WritePriority::Ingress, true);
        assert_eq!(module.sweep_slot_count_for_test(), 2);
        guard.armed = false;
        module.force_sync_pending_chunks()?;
        assert_eq!(module.sweep_slot_count_for_test(), 0);
        assert_data_byte_durable(&module, 0, 0x23)?;
        assert_data_byte_durable(&module, 1, 0x14)?;
        assert_data_byte_durable(&module, 2, 0x34)?;
        Ok(())
    }

    /// A one-chunk hole joins a parked offset to a durable neighbor. The
    /// memory slot between them does not split the span. The per-slot check
    /// stays false because the neighbor is not in `pending` or
    /// `written_ahead`. The whole span still waits on the parked write, so
    /// the drain must return. The force flush then commits that write and
    /// packs every body.
    #[test]
    fn coalesced_blocked_neighbor_does_not_hold_the_drain() -> eyre::Result<()> {
        let (_tmp, module) =
            span_fixture_timed("coalesced_blocked_neighbor", 4, 4, 1, 0, 0, 60_000)?;
        let parked = indexed_chunk(&module, 0, 0x42, 0x01)?;
        let ingress = indexed_chunk(&module, 1, 0x43, 0x05)?;
        let neighbor = indexed_chunk(&module, 2, 0x44, 0x07)?;
        assert!(module.write_chunk(
            PartitionChunkOffset::from(2),
            vec![0x33; 32],
            super::super::ChunkType::Entropy
        ));
        module.sync_pending_chunks_inner(true)?;
        assert!(module.write_chunk(
            PartitionChunkOffset::from(0),
            vec![0x22; 32],
            super::super::ChunkType::Entropy
        ));
        module.commit_pending_runs(false, Some(1), false)?;
        {
            let pending = module.pending_writes.read().unwrap();
            assert!(
                pending
                    .written_ahead
                    .contains_key(&PartitionChunkOffset::from(0)),
                "the parked offset was not parked"
            );
            assert!(
                !pending
                    .written_ahead
                    .contains_key(&PartitionChunkOffset::from(2)),
                "the neighbor is parked"
            );
            assert!(
                pending.get(&PartitionChunkOffset::from(2)).is_none(),
                "the neighbor is still in pending.chunks"
            );
        }
        let durable_left = module
            .durable_point_remaining()
            .expect("parked bytes wait on the durable point");
        assert!(
            !durable_left.is_zero(),
            "the durable point is already due: {durable_left:?}"
        );
        assert!(module.write_chunk(
            PartitionChunkOffset::from(1),
            vec![0x11; 32],
            super::super::ChunkType::Entropy
        ));
        module.deposit_data_chunks(&[parked, neighbor])?;
        let queued = module.enqueue_ingress_batch(std::slice::from_ref(&ingress))?;
        assert!(
            matches!(
                queued.first(),
                Some(super::super::BatchEnqueueItem::Queued(_))
            ),
            "{queued:?}"
        );
        assert_eq!(module.sweep_slot_count_for_test(), 3);
        assert_slot_shape(&module, 0, super::WritePriority::Migration, false);
        assert_slot_shape(&module, 1, super::WritePriority::Ingress, true);
        assert_slot_shape(&module, 2, super::WritePriority::Migration, false);
        assert!(
            !module.only_pending_blocked_disk_slots(),
            "the per-slot check saw the coalesced neighbor"
        );
        assert!(
            module.read_grace_remaining().is_none(),
            "a coalesced span that waits on a write is inside the read grace"
        );
        assert!(
            !module.memory_entropy_can_fold(),
            "the lower memory pad can fold ahead of the disk span"
        );
        assert!(
            !module.has_ready_disk_span(),
            "the coalesced span is ready to read"
        );
        assert!(
            !module.sweep_one(),
            "sweep read a span that waits on a write"
        );
        assert_eq!(module.disk.entropy_preads.load(Ordering::SeqCst), 0);
        assert_eq!(module.sweep_slot_count_for_test(), 3);
        let mut guard = FailSweepOnDrop {
            module: &module,
            armed: true,
        };
        let started = Instant::now();
        module.drive_entropy_queue_for_test();
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "drain waited {elapsed:?} on a coalesced span that waits on a write"
        );
        assert_eq!(module.disk.entropy_preads.load(Ordering::SeqCst), 0);
        assert_eq!(
            module.sweep_slot_count_for_test(),
            3,
            "the drain dropped a slot the parked write still blocks"
        );
        guard.armed = false;
        module.force_sync_pending_chunks()?;
        assert_eq!(module.sweep_slot_count_for_test(), 0);
        assert_data_byte_durable(&module, 0, 0x23)?;
        assert_data_byte_durable(&module, 1, 0x14)?;
        assert_data_byte_durable(&module, 2, 0x34)?;
        Ok(())
    }

    /// Memory at the best priority still folds while a lower disk slot is
    /// blocked. The drain packs that pad and returns without the sweep wait.
    #[test]
    fn higher_memory_still_folds_while_a_lower_disk_slot_is_blocked() -> eyre::Result<()> {
        let (_tmp, module) = span_fixture_timed("higher_memory_fold", 4, 4, 0, 0, 0, 0)?;
        let ingress = indexed_chunk(&module, 0, 0x42, 0x01)?;
        let migration = indexed_chunk(&module, 1, 0x43, 0x05)?;
        assert!(module.write_chunk(
            PartitionChunkOffset::from(0),
            vec![0x11; 32],
            super::super::ChunkType::Entropy
        ));
        module.sync_pending_chunks_inner(true)?;
        let queued = module.enqueue_ingress_batch(std::slice::from_ref(&ingress))?;
        assert!(
            matches!(
                queued.first(),
                Some(super::super::BatchEnqueueItem::Queued(_))
            ),
            "{queued:?}"
        );
        assert!(module.write_chunk(
            PartitionChunkOffset::from(0),
            vec![0x22; 32],
            super::super::ChunkType::Entropy
        ));
        assert!(module.write_chunk(
            PartitionChunkOffset::from(1),
            vec![0x11; 32],
            super::super::ChunkType::Entropy
        ));
        module.deposit_data_chunk(&migration)?;
        assert_eq!(module.sweep_slot_count_for_test(), 2);
        assert_slot_shape(&module, 0, super::WritePriority::Ingress, false);
        assert_slot_shape(&module, 1, super::WritePriority::Migration, true);
        assert!(module.memory_entropy_can_fold());
        assert!(!module.only_pending_blocked_disk_slots());
        let _guard = FailSweepOnDrop {
            module: &module,
            armed: true,
        };
        let started = Instant::now();
        module.drive_entropy_queue_for_test();
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "drain waited {elapsed:?} on a memory pad it could fold"
        );
        let pending = module.pending_writes.read().unwrap();
        let packed = pending
            .get(&PartitionChunkOffset::from(1))
            .map(|(bytes, kind)| (*kind, bytes.first().copied()));
        assert_eq!(
            packed,
            Some((super::super::ChunkType::Data, Some(0x14))),
            "the higher memory pad was not packed: {packed:?}"
        );
        drop(pending);
        assert_eq!(module.sweep_slot_count_for_test(), 1);
        assert_slot_shape(&module, 0, super::WritePriority::Ingress, false);
        assert_eq!(module.disk.entropy_preads.load(Ordering::SeqCst), 0);
        Ok(())
    }

    /// Lane shutdown's last flush is the crash window. The body has to be
    /// on disk when the lane thread returns, before `StorageModule::drop`.
    #[test]
    fn finish_disk_lane_packs_a_blocked_reread() -> eyre::Result<()> {
        let (_tmp, module, _chunk) = queue_blocked_reread("finish_blocked")?;
        module.finish_disk_lane();
        assert_replacement_body_durable(&module)
    }

    /// The lane signals the force caller when the handoff finishes. The
    /// reread has to be durable at that return, not after a later pass.
    #[test]
    fn lane_force_sync_finishes_a_blocked_reread_before_return() -> eyre::Result<()> {
        let (_tmp, module, _chunk) = queue_blocked_reread("lane_force_blocked")?;
        let module = Arc::new(module);
        let lane = StopLane::start(Arc::clone(&module))?;
        module.force_sync_pending_chunks()?;
        assert_replacement_body_durable(&module)?;
        drop(lane);
        Ok(())
    }

    /// A cached pad with no later write at its offset still packs and the
    /// force flush still makes that body durable.
    #[test]
    fn force_sync_still_durably_packs_an_unblocked_cached_pad() -> eyre::Result<()> {
        let (_tmp, module, chunk) = cache_module("force_unblocked")?;
        cache_durable_entropy(&module, 0x11)?;
        let queued = module.enqueue_ingress_batch(std::slice::from_ref(&chunk))?;
        assert!(
            matches!(
                queued.first(),
                Some(super::super::BatchEnqueueItem::Queued(_))
            ),
            "{queued:?}"
        );
        module.force_sync_pending_chunks()?;
        let offset = PartitionChunkOffset::from(0);
        assert!(module.is_data_chunk_durable_at(offset));
        assert!(module.is_data_chunk_persisted_at(offset));
        assert_eq!(module.sweep_slot_count_for_test(), 0);
        assert!(pending_head(&module).is_none());
        assert_eq!(chunk_file_byte(&module, offset)?, 0x10);
        Ok(())
    }

    /// Parked entropy blocks the only disk slot. No read is due. The lane
    /// sleeps until the durable point, so that wait is not read grace.
    #[test]
    fn blocked_slot_idle_reason_is_durable_point() -> eyre::Result<()> {
        let (_tmp, module) = span_fixture_timed("idle_blocked", 4, 4, 0, 0, 0, 60_000)?;
        let offset = PartitionChunkOffset::from(0);
        {
            let mut pending = module.pending_writes.write().unwrap();
            pending.written_ahead.insert(
                offset,
                super::super::WrittenAhead {
                    bytes: vec![0x22; 32],
                    chunk_type: super::super::ChunkType::Entropy,
                    priority: super::WritePriority::Packing,
                    queued_at: Instant::now(),
                    pwrite_at: Some(Instant::now()),
                    parked_at: Instant::now(),
                },
            );
        }
        insert_slots(&module, &[0], Instant::now());
        *module.last_durable_at.lock().unwrap() = Some(Instant::now());
        assert!(
            module.read_grace_remaining().is_none(),
            "a blocked slot is not a due read"
        );
        let remain = module
            .durable_point_remaining()
            .expect("parked bytes wait on the durable point");
        assert!(
            !remain.is_zero(),
            "the durable point is already due: {remain:?}"
        );
        assert_eq!(
            module.disk_idle_reason(),
            "durable_point",
            "idle reason {} while read grace is None",
            module.disk_idle_reason()
        );
        assert_eq!(module.lane_sleep_reason(), Some("durable_point"));
        Ok(())
    }

    /// A young short span is a real read wait. The label stays `read_grace`.
    #[test]
    fn young_span_idle_reason_stays_read_grace() -> eyre::Result<()> {
        let (_tmp, module) = span_fixture_grace("idle_young", 4, 4, 0, 1_000, 0)?;
        insert_slots(&module, &[0], Instant::now());
        let remain = module.read_grace_remaining().expect("the span is young");
        assert!(!remain.is_zero(), "the young span is already due");
        assert_eq!(module.disk_idle_reason(), "read_grace");
        assert_eq!(module.lane_sleep_reason(), Some("read_grace"));
        // Drop drains the queue. Leave it empty so that drain does not wait
        // out this span's grace.
        module.fail_queued_sweeps();
        Ok(())
    }

    /// A span whose grace has elapsed is due. That is still a read, not a
    /// durable-point wait.
    #[test]
    fn due_read_idle_reason_stays_read_grace() -> eyre::Result<()> {
        let (_tmp, module) = span_fixture("idle_due", 4, 4, 0)?;
        insert_slots(&module, &[0], Instant::now());
        assert_eq!(module.read_grace_remaining(), Some(Duration::ZERO));
        assert_eq!(module.disk_idle_reason(), "read_grace");
        assert_eq!(module.lane_sleep_reason(), Some("read_grace"));
        Ok(())
    }

    fn ingress_stage_elapsed(module: &super::super::StorageModule, stage: &str) -> Vec<Duration> {
        module
            .disk
            .ingest_stages
            .lock()
            .unwrap()
            .iter()
            .filter(|(name, _)| *name == stage)
            .map(|(_, elapsed)| *elapsed)
            .collect()
    }

    /// The disk compare runs after the XOR, under the chunk-file lock.
    /// That wait is not part of `entropy_to_packed`.
    #[test]
    fn entropy_to_packed_excludes_the_disk_compare() -> eyre::Result<()> {
        let (_tmp, module, chunk) = cache_module("stage_compare")?;
        cache_durable_entropy(&module, 0x11)?;
        module.entropy_write_epoch.fetch_add(1, Ordering::SeqCst);
        let module = Arc::new(module);
        module
            .disk
            .entropy_pack_pause
            .arm(super::EntropyPackPause::BeforePackXor);
        let hold = Duration::from_millis(200);
        thread::scope(|scope| -> eyre::Result<()> {
            let worker = scope.spawn(|| module.write_data_chunk(&chunk));
            assert!(
                module
                    .disk
                    .entropy_pack_pause
                    .wait_arrived(Duration::from_secs(5)),
                "the pack did not reach the pre-XOR pause"
            );
            let (file, _, _) = module.locate_io(PartitionChunkOffset::from(0), true)?;
            let locked = module.disk.lock_chunks(&file);
            module.disk.entropy_pack_pause.release();
            thread::sleep(hold);
            drop(locked);
            worker.join().expect("pack thread")?;
            Ok(())
        })?;
        assert_fresh_pad_packed(&module);
        let samples = ingress_stage_elapsed(&module, "entropy_to_packed");
        assert_eq!(samples.len(), 1, "{samples:?}");
        assert!(
            samples[0] < hold / 2,
            "entropy_to_packed included the disk compare: {:?} hold {hold:?}",
            samples[0]
        );
        Ok(())
    }

    /// A disk read that fails the byte check is queued again. Only the
    /// read that packs records `queue_to_entropy`.
    #[test]
    fn rejected_disk_read_records_queue_to_entropy_once() -> eyre::Result<()> {
        let (_tmp, module) = span_fixture("stage_reject", 4, 4, 0)?;
        let offset = PartitionChunkOffset::from(0);
        assert!(module.write_chunk(offset, vec![0x11; 32], super::super::ChunkType::Entropy));
        module.sync_pending_chunks_inner(true)?;
        let enqueued_at = Instant::now();
        let mut slot = sweep_slot_at(0, enqueued_at);
        slot.unpacked = Arc::new(vec![0x01; 32]);
        slot.byte_len = 32;
        slot.entropy_epoch = 0;
        {
            let mut queue = module.disk.queue();
            queue.groups.insert(
                slot.group,
                super::SweepGroup {
                    remaining: 1,
                    failed: None,
                    waited: false,
                    notify: None,
                },
            );
        }
        module
            .pending_writes
            .write()
            .unwrap()
            .occupancy
            .insert(offset, slot.generation);
        module.disk.ingest_stages.lock().unwrap().clear();
        module.submit_packed(slot, vec![0x99; 32], Some(enqueued_at));
        assert_eq!(
            module.sweep_slot_count_for_test(),
            1,
            "the rejected read was not queued again"
        );
        let after_reject = ingress_stage_names(&module);
        assert!(
            !after_reject.contains(&"queue_to_entropy"),
            "the rejected read recorded queue_to_entropy: {after_reject:?}"
        );
        assert!(module.sweep_one(), "the reread did not pack");
        let stages = ingress_stage_names(&module);
        let queue_samples = stages
            .iter()
            .filter(|stage| **stage == "queue_to_entropy")
            .count();
        let packed_samples = stages
            .iter()
            .filter(|stage| **stage == "entropy_to_packed")
            .count();
        assert_eq!(queue_samples, 1, "{stages:?}");
        assert_eq!(packed_samples, 1, "{stages:?}");
        Ok(())
    }
}
