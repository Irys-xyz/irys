//! One thread per storage module locks `chunks.dat`. Order is mining recall,
//! then a ready packed write, then an entropy read. A recall does not cancel
//! a `pwrite` already in the kernel, and this thread submits no new write
//! while `recall_waiters` is set.
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

use super::{
    BatchEnqueueItem, ChunkType, StorageModule, WriteDataChunkError, WriteRun,
    index_drain::{ChunkBusy, IndexGap},
};

/// How long a caller waits for its entropy read before it gives up.
const SWEEP_WAIT: Duration = Duration::from_secs(30);
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
    /// When this slot joined the sweep queue. A short span waits from the
    /// oldest slot, so a neighbor does not restart the grace.
    enqueued_at: Instant,
    /// Entropy taken from the read-ahead cache. Not pending-map entropy:
    /// the interval is checked again before the XOR.
    cached_entropy: Option<Vec<u8>>,
    /// `entropy_write_epoch` at the pread. A later entropy pwrite must not
    /// be packed from this buffer.
    entropy_epoch: u64,
}

/// One reserved offset. The third field is entropy already in `pending`.
/// The fourth is a read-ahead cache pad. Only one of them is set.
type SelectedOffset = (PartitionChunkOffset, u64, Option<Vec<u8>>, Option<Vec<u8>>);

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
    fn take(&mut self, offset: PartitionChunkOffset, generation: u64) -> Option<Vec<u8>> {
        let pad = self.remove(offset)?;
        (pad.generation == generation).then_some(pad.bytes)
    }

    /// Keep `bytes` for `offset`. Evict the oldest pads until it fits.
    /// A pad larger than `cap` is not stored. Returns whether it was stored.
    fn insert(
        &mut self,
        offset: PartitionChunkOffset,
        bytes: Vec<u8>,
        generation: u64,
        cap: u64,
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
    wake: DiskWake,
    queue: Mutex<SweepQueue>,
    sweep_busy: Mutex<()>,
    /// The service thread owns sweeps and flushes while this is set.
    lane_started: AtomicBool,
    lane_stop: AtomicBool,
    /// Index commits wait while a chunk run or a mining recall holds this.
    index_gap: Arc<IndexGap>,
    /// Set for the life of `run_disk_lane`. External flushes use it to avoid
    /// waiting for themselves.
    lane_thread: Mutex<Option<std::thread::ThreadId>>,
    /// External flush and interval writes wait here. The lane thread runs them.
    handoff: LaneHandoff,
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
    #[cfg(test)]
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
            lane_thread: Mutex::new(None),
            handoff: LaneHandoff::new(),
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

    /// Recall, a file holder, and a full service budget all block a new submit.
    fn disk_available(&self) -> bool {
        self.recall_waiters.load(Ordering::SeqCst) == 0
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

    /// No recall, write hold, exclusive IO, or in-kernel chunk command.
    /// A short packed write waits for a neighbor only while this is false.
    /// A short entropy span may wait while this is true.
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
        if self.disk.recall_pending() || self.disk.writes_active() {
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
                // check and the take.
                let cached_entropy = if pending_entropy.is_none() {
                    self.entropy_cache
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .take(partition_offset, generation)
                } else {
                    None
                };
                if cached_entropy.is_some() {
                    super::super::index_read_metrics::note_readahead_hit();
                    #[cfg(any(test, feature = "bench"))]
                    self.disk
                        .entropy_readahead_hits
                        .fetch_add(1, Ordering::SeqCst);
                }
                selected.push((
                    partition_offset,
                    generation,
                    pending_entropy,
                    cached_entropy,
                ));
            }
            if selected.is_empty() {
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
                    .any(|(_, generation, _, _)| *generation != generation_now)
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
            for (offset, generation, pending_entropy, cached_entropy) in chunk.selected {
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
                    entropy_epoch: 0,
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
            // Recall can stay queued for the whole step. Stop this waiter on
            // the wall clock so the wait does not run until recall goes idle.
            // A group that finished during the wait keeps that result.
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
            if self.entropy_idle() {
                return;
            }
            self.disk.yield_to_recall();
            let folded = self.fold_memory_entropy();
            let swept = self.sweep_one();
            if folded || swept {
                idle_deadline = None;
                continue;
            }
            let seen = self.disk.wake.epoch();
            if self.entropy_idle() {
                return;
            }
            let overall = *idle_deadline.get_or_insert_with(|| Instant::now() + SWEEP_WAIT);
            // A young short span is not eligible yet. Sleep only until the
            // grace ends, then issue it. The sweep give-up is still the bound
            // when the span never becomes eligible.
            let wake_by = self
                .read_grace_remaining()
                .filter(|delay| !delay.is_zero())
                .and_then(|delay| Instant::now().checked_add(delay))
                .map(|at| at.min(overall))
                .unwrap_or(overall);
            if self.wait_until_epoch_advances(seen, wake_by) && Instant::now() >= overall {
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

    /// Fail a queued sweep slot at `offset`. The copied pad is stale.
    /// One map lookup. A scan of the backlog on every chunk write tracked
    /// the queue length.
    pub(super) fn fail_sweep_offset(&self, offset: PartitionChunkOffset) {
        let removed = {
            let mut queue = self.disk.queue();
            queue.remove_slot(offset)
        };
        if let Some(slot) = removed {
            self.release_queued_offset(slot.offset, slot.generation, slot.byte_len);
            self.mark_group_slot_done(slot.group, Some(WriteDataChunkError::Backpressure));
        }
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
            if self.disk.recall_pending() {
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
        if self.disk.recall_pending() || self.disk.writes_active() {
            return false;
        }
        let chunk_size = self.config.consensus.chunk_size.max(1);
        let Some(span) = self.take_ready_span() else {
            return false;
        };
        if self.disk.recall_pending() || self.disk.writes_active() {
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
        self.note_issued_entropy_pread(span.len(), buf.len() as u64);
        if let Err(error) = file.read_exact_at(&mut buf, file_offset) {
            drop(file);
            let error = WriteDataChunkError::Other(eyre::eyre!(
                "entropy sweep at offset {start} count {span_chunks}: {error}"
            ));
            self.fail_span(span, &error);
            return true;
        }
        // Captured before the file lock drops. A later entropy pwrite needs
        // that lock, so it cannot land and then hide behind this value.
        let entropy_epoch = self.entropy_write_epoch.load(Ordering::SeqCst);
        drop(file);
        #[cfg(test)]
        self.wait_if_sync_paused(super::SyncPausePoint::AfterEntropyPread);
        self.finish_span_read(span, start, &buf, entropy_epoch);
        true
    }

    /// Entropy preads up to the service budget, in offset order. A short span
    /// is included once it is past the read grace or it already meets the
    /// batch target. Recall, a packed-write hold, and the in-flight budget
    /// still stop the pass. The lane thread XORs and submits the index only
    /// after those calls return.
    fn sweep_inflight(&self) {
        if self.disk.recall_pending() || self.disk.writes_active() {
            return;
        }
        let chunk_size = self.config.consensus.chunk_size.max(1);
        let mut spans = Vec::new();
        let mut read_ends = Vec::new();
        let mut guards = Vec::with_capacity(INFLIGHT_WRITES);
        while spans.len() < INFLIGHT_WRITES {
            if self.disk.recall_pending() || self.disk.writes_active() {
                break;
            }
            let Some(span) = self.take_ready_span() else {
                break;
            };
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
                Ok(load) => self.finish_span_read(span, load.start, &load.buf, load.entropy_epoch),
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
        Ok(SpanLoad {
            start,
            buf,
            entropy_epoch,
        })
    }

    fn finish_span_read(
        &self,
        span: Vec<SweepSlot>,
        start: PartitionChunkOffset,
        buf: &[u8],
        entropy_epoch: u64,
    ) {
        let read_done = Instant::now();
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
            if cache.insert(offset, buf[at..at + chunk_len].to_vec(), generation, cap) {
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
        if self.entropy_read_grace().is_zero() {
            return true;
        }
        self.first_ready_span(&queue).is_some()
    }

    /// Offsets of the lowest-offset ready disk span. `None` when every disk
    /// span is still inside the read grace.
    fn first_ready_span(&self, queue: &SweepQueue) -> Option<Vec<PartitionChunkOffset>> {
        let chunk = self.config.consensus.chunk_size.max(1);
        let mut found = None;
        self.for_each_disk_span(&queue.slots, |span| {
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
        if let Err(error) = self.slot_still_valid(&slot) {
            self.fail_slot(slot, error);
            self.fail_queued_group(group);
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
        if priority == WritePriority::Ingress
            && let Some(read_done) = read_done
        {
            super::super::index_read_metrics::note_ingest_stage(
                "queue_to_entropy",
                read_done.saturating_duration_since(enqueued_at),
            );
        }
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
        if let Err(error) = self.slot_still_valid(&slot) {
            self.fail_slot(slot, error);
            self.fail_queued_group(group);
            return;
        }
        // The data-path row is written after `chunks.dat` is fdatasync'd.
        // Failing here leaves no pending bytes and no index row.
        #[cfg(any(test, feature = "test-utils"))]
        if self.take_index_commit_failure() {
            self.fail_slot(
                slot,
                WriteDataChunkError::Other(eyre::eyre!("injected index drain commit failure")),
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
        let packed = packing_xor_vec_u8(entropy, &slot.unpacked);
        let inserted = self.commit_packed(
            slot.offset,
            packed,
            slot.generation,
            slot.priority,
            slot.byte_len,
            (slot.path_hash, (*slot.data_path).clone()),
            copied_pad,
        );
        if inserted.is_ok() && priority == WritePriority::Ingress && read_done.is_some() {
            super::super::index_read_metrics::note_ingest_stage(
                "entropy_to_packed",
                pack_started.elapsed(),
            );
        }
        self.mark_group_slot_done(group, inserted.err());
    }

    fn slot_still_valid(&self, slot: &SweepSlot) -> Result<(), WriteDataChunkError> {
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
        }
        if slot.pending_entropy.is_some() {
            return Ok(());
        }
        // The pad was read from disk. An entropy pwrite since that read has
        // newer bytes. Retry instead of packing the old pad.
        if slot.cached_entropy.is_none()
            && self.entropy_write_epoch.load(Ordering::SeqCst) != slot.entropy_epoch
        {
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
    ) -> Result<(), WriteDataChunkError> {
        let mut pending = self.pending_writes.write().unwrap();
        pending.queued_unpacked_bytes = pending.queued_unpacked_bytes.saturating_sub(unpacked_len);
        if pending.occupancy.get(&offset).copied() != Some(generation) {
            return Err(WriteDataChunkError::WritesPaused);
        }
        // `written_ahead` is a pwrite this commit did not do. Leave it.
        // A copied pad must not replace entropy that landed after the copy.
        if pending.written_ahead.contains_key(&offset)
            || (copied_pad && pending.contains_key(&offset))
        {
            pending.occupancy.remove(&offset);
            return Err(WriteDataChunkError::Backpressure);
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
        Ok(())
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
        if self.disk.recall_pending() {
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
        if self.disk.queue().disk_len() > 0 {
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
    /// stays busy.
    fn write_grace_remaining(&self) -> Option<Duration> {
        let pending = self.pending_writes.read().unwrap();
        let oldest = pending.queued_at.values().copied().min()?;
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
        if grace.is_zero() {
            return Some(Duration::ZERO);
        }
        let chunk = self.config.consensus.chunk_size.max(1);
        let mut seen = false;
        let mut ready = false;
        let mut soonest = grace;
        self.for_each_disk_span(&queue.slots, |span| {
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
        // Recall sets `recall_waiters` before it reads, and it already pinned
        // the index gap. Submit nothing until that hold drops.
        if self.disk.recall_pending() {
            return;
        }
        // Completion rewrites wait out an active recall. They do not take the
        // chunk-io hold, so a recall queued behind them is not blocked on the
        // interval file.
        if let Err(error) = self.flush_interval_completions_locked(false) {
            tracing::error!(
                "Couldn't rewrite intervals for storage_module {}: {error}",
                self.id
            );
        }
        // The index drain stays paused for this pass. Presence rows commit on
        // this thread after `chunks.dat` is fdatasync'd. Submitting them to
        // the drain here would wait on this hold.
        let chunk_io = self.disk.index_gap.hold_for_chunk_io();
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
        if !owed_flush && !self.disk.recall_pending() {
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

    /// An `Instant` this far in the past. The clock cannot go backwards.
    fn ago(age: Duration) -> Instant {
        Instant::now()
            .checked_sub(age)
            .expect("test age fits in Instant")
    }

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
                intervals_persist_secs: 0,
                index_wal_backstop_ms: irys_types::DEFAULT_INDEX_WAL_BACKSTOP_MS,
                tx_path_derive_threads: irys_types::DEFAULT_TX_PATH_DERIVE_THREADS,
                min_durable_interval_ms: 0,
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
            selected: vec![(PartitionChunkOffset::from(0), 1, None, None)],
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
        let refused = module.commit_packed(
            offset,
            vec![0x11; 32],
            0,
            super::WritePriority::Ingress,
            32,
            (H256::zero(), Vec::new()),
            true,
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
                assert!(cache.insert(PartitionChunkOffset::from(index), vec![1; 32], 1, cap,));
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
            assert!(cache.insert(PartitionChunkOffset::from(index), vec![1; 32], 1, cap,));
        }
        assert!(cache.insert(PartitionChunkOffset::from(0), vec![2; 32], 1, cap));
        assert!(cache.take(PartitionChunkOffset::from(0), 1).is_some());
        assert!(cache.insert(PartitionChunkOffset::from(2), vec![3; 32], 1, cap));
        assert!(cache.insert(PartitionChunkOffset::from(3), vec![4; 32], 1, cap));
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
}
