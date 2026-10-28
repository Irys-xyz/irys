use crate::{
    DataSyncServiceMessage,
    chunk_fetcher::{ChunkFetchError, ChunkFetcher, DataIntervalsPage},
    chunk_migration_service::validate_chunk_for_migration,
    data_sync_service::{
        assignee_intervals::{
            self, AssigneeIntervals, Coverage, HINT_FRONTIER_MARGIN, HINT_PAGE_TIMEOUT, WindowFetch,
        },
        peer_bandwidth_manager::PeerBandwidthManager,
    },
    metrics,
    services::ServiceSenders,
};
use irys_database::{cached_chunk_by_chunk_offset, db::IrysDatabaseExt as _};
use irys_domain::{
    BlockTreeReadGuard, ChunkTimeRecord, ChunkType, CircularBuffer, StorageModule,
    WriteDataChunkError,
};
use irys_types::{
    DataLedger, IrysAddress, LedgerChunkOffset, NodeConfig, PartitionChunkOffset, SendTraced as _,
    UnpackedChunk, app_state::DatabaseProvider, hardfork_config::DataLedgerLookup,
};
use nodit::interval::ii;
use std::{
    collections::{HashMap, HashSet, VecDeque, hash_map},
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};
use tracing::{Instrument as _, debug, warn};

/// Prefer residual holes below the ordinary packing tail when sampling data
/// roots for ingress-proof signer peer expansion.
pub(crate) const LOW_OFFSET_PROBE_THRESHOLD: u32 = 20_000;

/// Durability normally follows the five-second idle-tail flush. A much larger
/// monotonic deadline prevents a stalled write from parking sync forever
/// without turning ordinary buffered writes into duplicate network requests.
const DURABILITY_WAIT_TIMEOUT: Duration = Duration::from_secs(60);

/// Longest wait for a write the storage module still holds in its write path
/// (queued, buffered, or written but not fsynced). A busy disk can take longer
/// than [`DURABILITY_WAIT_TIMEOUT`] to flush, and fetching the chunk again then
/// only finds the same pending write. Past this, the chunk returns to the
/// fetch queue anyway.
const DURABILITY_PENDING_LIMIT: Duration = Duration::from_secs(10 * 60);

/// Retry an offset only after other ready work has had an opportunity to run.
/// While its in-memory request record is retained, the delay is monotonic,
/// doubles after each exhausted peer cycle, and is capped so data that appears
/// later is eventually rediscovered. Jitter adds up to a quarter of each step.
/// At the cap the window opens below it, so a capped retry waits 45 to 60
/// seconds.
const RETRY_BACKOFF_INITIAL: Duration = Duration::from_secs(1);
const RETRY_BACKOFF_MAX: Duration = Duration::from_secs(60);

/// Bound scheduler bookkeeping independently from the partition's Entropy
/// intervals, which remain the durable source of unresolved work on restart.
/// When this many delayed entries are retained, the entry with the soonest
/// retry deadline is discarded because it has the least suppression left.
/// This retains the longer backoff for repeat offenders while the rotating
/// discovery cursor eventually rediscovers the discarded Entropy offset. As
/// on restart, rediscovery resets that offset's ephemeral backoff.
const DELAYED_REQUEST_LIMIT_MULTIPLIER: usize = 1;

/// Blocked offsets are local progress blockers, not a second hot queue.
/// Keep a few budgets of the lowest offsets for re-arm. On a term ledger also
/// keep the write head: dropping it would enqueue it again on the next tick.
/// Every offset that does not fit stays in the interval map and is found again
/// when the scan cursor wraps.
const BLOCKED_REQUEST_LIMIT_MULTIPLIER: usize = 4;

/// One attempt to start a fetch for a ready offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DispatchStart {
    /// A fetch task was spawned.
    Started,
    /// This offset cannot start now. The caller tries the next ready offset.
    Skip,
    /// The local ledger range is gone. The offset is back on the ready
    /// queue, and the same peer would be selected again this round.
    Stop,
}

/// How to request a missing body from a selected peer.
///
/// Assignees use ledger-offset; ingress-proof signers (often no SM) use the data commitment.
#[derive(Debug, Clone, Copy)]
enum FetchMode {
    LedgerOffset(LedgerChunkOffset),
    DataCommitment {
        data_commitment: irys_types::DataCommitment,
        tx_offset: irys_types::TxChunkOffset,
    },
}

/// Why a chunk offset is blocked from the hot re-fetch loop.
///
/// These are local progress blockers (not peer-delivery failures). Peers may
/// have delivered the bytes; we still cannot place them until the underlying
/// issue is repaired (e.g. SM data_root index rebuild).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkBlockReason {
    /// `write_data_chunk` failed because `DataRootInfosByDataRoot` has no entry
    /// for the chunk's data_root. Needs index rebuild / backfill.
    MissingDataRootIndex,
}

impl ChunkBlockReason {
    pub const fn as_metric_label(self) -> &'static str {
        match self {
            Self::MissingDataRootIndex => "missing_data_root_index",
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum ChunkRequestState {
    /// Chunk is at the back of the fair dispatch queue. `ChunkRequest::ready_since`
    /// records its monotonic queue age for observability.
    Pending,

    /// Chunk has been requested from the specified peer at the given timestamp.
    /// Used for tracking timeouts and preventing duplicate requests.
    Requested(IrysAddress, Instant),

    /// Peer delivery was accepted into the storage module's pending-write
    /// buffer. Advanced to `Completed` once the storage module reports the body
    /// as fsynced, polled every tick by `reconcile_awaiting_durability`.
    AwaitingDurability(Instant),

    /// Chunk was successfully fetched **and** durably written as [`ChunkType::Data`].
    ///
    /// Fetch alone is not enough — see [`Self::on_chunk_fetched`] then
    /// [`Self::mark_chunk_awaiting_durability`], then the durability poll in
    /// `reconcile_awaiting_durability`.
    Completed,

    /// Every currently eligible peer has failed this attempt cycle. The offset
    /// remains unresolved but cannot consume a fetch slot before `retry_at`.
    Delayed { retry_at: Instant },

    /// Locally blocked from hot re-fetch (index gap, etc.). Still retained while
    /// the offset is Entropy so we do not thrash peers. Cleared when the offset
    /// becomes Data, when a re-arm tick finds the local index ready for this
    /// offset ([`ChunkOrchestrator::unblock_missing_data_root_index_where`]), or
    /// on process restart.
    Blocked(ChunkBlockReason),

    /// A live body-migration job covers this offset. Data sync does not fetch
    /// or write it. A later tick returns it to [`Self::Pending`] once the job
    /// is gone, the hold times out, or the offset is no longer Entropy.
    HeldByBodyWorker,
}

/// One offset's body-worker hold.
///
/// `since` is monotonic time when the hold started, or when the job's row key
/// last moved forward. `anchor` is the coverage range start at that moment.
/// `expired` stays set while the same range still covers the offset, so the
/// hold is not taken again until the key moves or the job is gone.
#[derive(Clone, Copy, Debug)]
struct BodyHold {
    since: Instant,
    anchor: u32,
    expired: bool,
}

type ExcludedPeerAddresses = HashSet<IrysAddress>;
/// Per-offset sync request. Ledger context lives on the parent
/// [`ChunkOrchestrator`] (`self.ledger_id`); one orchestrator is one SM
/// assignment, so it is not duplicated per request.
#[derive(Debug)]
pub struct ChunkRequest {
    pub excluded: ExcludedPeerAddresses,
    pub request_state: ChunkRequestState,
    /// Number of network attempts during this in-memory scheduler lifetime.
    pub attempt_count: u32,
    /// Number of attempt cycles that exhausted every currently eligible peer.
    pub retry_round: u32,
    /// Monotonic time this offset most recently joined the FIFO ready queue.
    pub ready_since: Instant,
    /// A local-cache lookup already missed during this attempt cycle. Later
    /// pops skip the cache until the request is dropped, or until a delayed
    /// retry becomes pending. A body that arrives during the backoff is read
    /// on that next cycle. A same-cycle peer rotation does not clear this.
    pub local_cache_miss: bool,
    /// The peer delivered this `Requested` body and its slot is already
    /// released. Ingress runs in a spawned task, and the offset stays
    /// `Requested` until `ChunkIngressed` arrives. A peer disconnect or a
    /// slot sweep must not release that slot again. Only meaningful while the
    /// state is `Requested`; a new dispatch clears it.
    pub body_in_ingress: bool,
}

/// Result of one local-cache attempt for the offset `dispatch_next` popped.
enum LocalCacheStep {
    /// The start offset was accepted locally (buffered, durable, or blocked).
    Done,
    /// The start offset missed. The caller may ask a peer.
    Miss,
    /// The start offset was requeued after a write failure. Do not ask a peer
    /// on this tick, or the same write is retried immediately.
    Stop,
}

fn write_error_label(error: &WriteDataChunkError) -> &'static str {
    match error {
        WriteDataChunkError::WritesPaused => "writes_paused",
        WriteDataChunkError::Backpressure => "backpressure",
        WriteDataChunkError::DataRootNotFound => "missing_data_root_index",
        WriteDataChunkError::Other(_) => "other",
    }
}

/// Minimum gap between entropy-interval scans for an orchestrator without peers.
const ENTROPY_PEER_CHECK_INTERVAL: Duration = Duration::from_secs(10);

/// Orchestrates efficient chunk downloading for a StorageModule's assigned data partition.
///
/// Key responsibilities:
/// - Rate-limits chunk requests based on local StorageModules' disk write throughput
/// - Queues and dispatches chunk requests across available peers
/// - Optimizes concurrency using peer health scores from PeerBandwidthManagers
/// - Tracks performance metrics for observability
#[derive(Debug)]
pub struct ChunkOrchestrator {
    pub chunk_requests: HashMap<PartitionChunkOffset, ChunkRequest>,
    /// Explicit FIFO ordering for ready work. Stale entries are skipped when a
    /// request changes state, keeping state ownership in `chunk_requests`.
    ready_offsets: VecDeque<PartitionChunkOffset>,
    /// Next partition-relative offset considered while discovering Entropy.
    /// Publish walks low-to-high from here. Term ledgers walk high-to-low
    /// (`u32::MAX` means "start at the write head") so abandoned low-offset
    /// gaps are visited only after the frontier has been scanned.
    scan_cursor: u32,
    /// Fingerprint of live Entropy intervals from the last peerless check.
    /// Zero means that pass saw no Entropy. A change while
    /// [`Self::current_peers`] is empty means this module had no peer managers
    /// when assignments were last synchronized. The interval list itself is
    /// not stored: a fragmented module can hold hundreds of thousands of spans.
    seen_entropy_fingerprint: u64,
    /// False until a peerless check has stored [`Self::seen_entropy_fingerprint`].
    /// A peer list clears this so the next peerless period treats Entropy that
    /// is already on the module as new.
    entropy_fingerprint_ready: bool,
    last_entropy_peer_check: Option<Instant>,
    pub current_peers: Vec<IrysAddress>,
    block_tree: BlockTreeReadGuard,
    pub storage_module: Arc<StorageModule>,
    recent_chunk_times: CircularBuffer<ChunkTimeRecord>, // Performance tracking for observability
    // Shared reference to peer bandwidth managers maintained by DataSyncService
    active_sync_peers: Arc<RwLock<HashMap<IrysAddress, PeerBandwidthManager>>>,
    service_senders: ServiceSenders,
    ledger_id: u32,
    chunk_fetcher: Arc<dyn ChunkFetcher>,
    config: NodeConfig,
    runtime_handle: tokio::runtime::Handle,
    /// Main node DB. Holds validated chunk bodies that have not reached this
    /// storage module yet.
    db: DatabaseProvider,
    /// Inclusive partition offsets a live body-migration job covers.
    /// One read per tick. Empty after a failed read, which fails open.
    pending_body_ranges: Vec<(u32, u32)>,
    /// This tick already tried to read coverage. A failed read stays true so
    /// dispatch does not read the table again. The next `prepare_tick` retries.
    coverage_loaded: bool,
    /// Holds keyed by partition offset. Absent when this offset was never held.
    body_holds: HashMap<PartitionChunkOffset, BodyHold>,
    /// Holds released because the row key did not move before the timeout.
    #[cfg(test)]
    hold_expired: u64,
    /// Coverage reads this orchestrator has attempted, including failures.
    #[cfg(test)]
    coverage_reads: u32,
    /// Holds this orchestrator started. Not the process-wide metric.
    #[cfg(test)]
    holds_started: u64,
    /// When set, the next coverage read fails without touching the database.
    #[cfg(test)]
    fail_coverage_read: bool,
    /// Local-cache lookups since the last [`Self::prepare_tick`].
    cache_lookups_this_tick: u32,
    /// What each assignee reports holding. The scan skips Entropy no assignee
    /// claims, and dispatch does not ask an assignee for an offset it answered
    /// without.
    pub(crate) assignee_intervals: AssigneeIntervals,
    /// Slot the hints were fetched for. A new slot starts from no hints.
    hint_slot: Option<usize>,
    /// The last scan walked the whole partition and found no Entropy. Interval
    /// hints are not fetched while this holds: there is nothing to skip.
    hints_idle: bool,
    /// Set by the scan when it finds an Entropy run this tick.
    scan_found_entropy: bool,
}
impl ChunkOrchestrator {
    pub fn new(
        storage_module: Arc<StorageModule>,
        sync_peers: Arc<RwLock<HashMap<IrysAddress, PeerBandwidthManager>>>,
        block_tree: BlockTreeReadGuard,
        service_senders: &ServiceSenders,
        chunk_fetcher: Arc<dyn ChunkFetcher>,
        config: NodeConfig,
        runtime_handle: tokio::runtime::Handle,
        db: DatabaseProvider,
    ) -> Self {
        let ledger_id = storage_module
            .partition_assignment()
            .expect("storage_module should have a partition assignment")
            .ledger_id
            .expect("storage_module should be assigned to a data ledger");

        Self::from_parts(
            storage_module,
            sync_peers,
            block_tree,
            service_senders,
            chunk_fetcher,
            config,
            runtime_handle,
            db,
            ledger_id,
        )
    }

    /// Builds only when the live assignment is still `ledger_id`.
    ///
    /// The fetcher is created for one ledger before this read. A clear or a
    /// ledger change between those two reads must not pair that fetcher with a
    /// different ledger. The caller retries on a later sync.
    pub(crate) fn try_new_for_ledger(
        storage_module: Arc<StorageModule>,
        sync_peers: Arc<RwLock<HashMap<IrysAddress, PeerBandwidthManager>>>,
        block_tree: BlockTreeReadGuard,
        service_senders: &ServiceSenders,
        chunk_fetcher: Arc<dyn ChunkFetcher>,
        config: NodeConfig,
        runtime_handle: tokio::runtime::Handle,
        db: DatabaseProvider,
        ledger_id: u32,
    ) -> Option<Self> {
        let live = storage_module
            .partition_assignment()
            .and_then(|pa| pa.ledger_id);
        if live != Some(ledger_id) {
            return None;
        }
        Some(Self::from_parts(
            storage_module,
            sync_peers,
            block_tree,
            service_senders,
            chunk_fetcher,
            config,
            runtime_handle,
            db,
            ledger_id,
        ))
    }

    fn from_parts(
        storage_module: Arc<StorageModule>,
        sync_peers: Arc<RwLock<HashMap<IrysAddress, PeerBandwidthManager>>>,
        block_tree: BlockTreeReadGuard,
        service_senders: &ServiceSenders,
        chunk_fetcher: Arc<dyn ChunkFetcher>,
        config: NodeConfig,
        runtime_handle: tokio::runtime::Handle,
        db: DatabaseProvider,
        ledger_id: u32,
    ) -> Self {
        let hint_slot = storage_module
            .partition_assignment()
            .and_then(|pa| pa.slot_index);
        Self {
            storage_module,
            chunk_requests: Default::default(),
            ready_offsets: Default::default(),
            scan_cursor: if Self::ledger_prioritizes_write_frontier(ledger_id) {
                u32::MAX
            } else {
                0
            },
            seen_entropy_fingerprint: 0,
            entropy_fingerprint_ready: false,
            last_entropy_peer_check: None,
            recent_chunk_times: CircularBuffer::new(8000),
            current_peers: Default::default(),
            block_tree,
            active_sync_peers: sync_peers,
            service_senders: service_senders.clone(),
            ledger_id,
            chunk_fetcher,
            config,
            runtime_handle,
            db,
            pending_body_ranges: Vec::new(),
            coverage_loaded: false,
            body_holds: HashMap::new(),
            #[cfg(test)]
            hold_expired: 0,
            #[cfg(test)]
            coverage_reads: 0,
            #[cfg(test)]
            holds_started: 0,
            #[cfg(test)]
            fail_coverage_read: false,
            cache_lookups_this_tick: 0,
            assignee_intervals: AssigneeIntervals::new(HINT_FRONTIER_MARGIN),
            hint_slot,
            hints_idle: false,
            scan_found_entropy: false,
        }
    }

    pub(crate) const fn ledger_id(&self) -> u32 {
        self.ledger_id
    }

    /// Track `offset` as a ready `Pending` request at the queue tail. An
    /// offset already tracked is left alone.
    #[cfg(test)]
    pub(crate) fn queue_pending_for_test(&mut self, offset: PartitionChunkOffset) {
        self.enqueue_pending(offset, Instant::now(), false);
    }

    /// Allocation of the fetcher this orchestrator dispatches with.
    #[cfg(test)]
    pub(crate) fn test_fetcher_addr(&self) -> usize {
        Arc::as_ptr(&self.chunk_fetcher) as *const () as usize
    }

    /// True when this pass saw an Entropy interval that was not present last
    /// pass and the orchestrator still has no peers. The caller syncs partition
    /// assignments for that module. The epoch refresh stays in place.
    pub(crate) fn note_new_entropy_without_peers(&mut self) -> bool {
        // Runs every tick. A fragmented module holds hundreds of thousands of
        // entropy intervals, so scan only when the answer can matter (no
        // peers) and at most once per ENTROPY_PEER_CHECK_INTERVAL. The scan
        // is a fingerprint of the live map, not a rebuilt interval list.
        if !self.current_peers.is_empty() {
            self.seen_entropy_fingerprint = 0;
            self.entropy_fingerprint_ready = false;
            return false;
        }
        let now = Instant::now();
        if self
            .last_entropy_peer_check
            .is_some_and(|last| now.duration_since(last) < ENTROPY_PEER_CHECK_INTERVAL)
        {
            return false;
        }
        self.last_entropy_peer_check = Some(now);
        let fingerprint = entropy_interval_fingerprint(&self.storage_module);
        let has_new = if self.entropy_fingerprint_ready {
            fingerprint != self.seen_entropy_fingerprint
        } else {
            fingerprint != 0
        };
        self.seen_entropy_fingerprint = fingerprint;
        self.entropy_fingerprint_ready = true;
        has_new
    }

    #[cfg(test)]
    pub(crate) fn entropy_peer_checked(&self) -> bool {
        self.last_entropy_peer_check.is_some()
    }

    /// Refresh local scheduler state. Dispatch is driven separately by
    /// `DataSyncServiceInner` so storage modules can share peer concurrency in
    /// round-robin order.
    pub fn prepare_tick(&mut self) {
        // Only need to tick the Orchestrator if the partition is still assigned to a ledger.
        // Capacity partitions don't need to sync data.
        if self
            .storage_module
            .partition_assignment()
            .is_some_and(|pa| pa.ledger_id.is_some())
        {
            self.cache_lookups_this_tick = 0;
            self.reload_pending_body_coverage();
            self.release_body_worker_holds(Instant::now());
            self.populate_request_queue();
        }
    }

    fn reload_pending_body_coverage(&mut self) {
        #[cfg(test)]
        {
            self.coverage_reads = self.coverage_reads.saturating_add(1);
            if self.fail_coverage_read {
                // A read error must not hide holes from peers, and must not
                // be retried on every dispatch in this tick.
                self.pending_body_ranges.clear();
                self.coverage_loaded = true;
                warn!("pending body coverage read failed; data sync will not skip offsets");
                return;
            }
        }
        match self.storage_module.pending_body_coverage_ranges() {
            Ok(ranges) => {
                self.pending_body_ranges = ranges;
            }
            Err(error) => {
                // A read error must not hide holes from peers, and must not
                // be retried on every dispatch in this tick.
                self.pending_body_ranges.clear();
                warn!(
                    error = %error,
                    "pending body coverage read failed; data sync will not skip offsets"
                );
            }
        }
        self.coverage_loaded = true;
    }

    fn covering_range(&self, offset: PartitionChunkOffset) -> Option<(u32, u32)> {
        let offset = *offset;
        let index = self
            .pending_body_ranges
            .binary_search_by(|range| {
                if range.1 < offset {
                    std::cmp::Ordering::Less
                } else if range.0 > offset {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .ok()?;
        Some(self.pending_body_ranges[index])
    }

    fn pending_body_covers(&self, offset: PartitionChunkOffset) -> bool {
        self.covering_range(offset).is_some()
    }

    /// The body worker still owns `offset`, and the hold has not timed out.
    fn body_worker_owns(&self, offset: PartitionChunkOffset) -> bool {
        self.pending_body_covers(offset)
            && !self
                .body_holds
                .get(&offset)
                .is_some_and(|hold| hold.expired)
    }

    fn hold_for_body_worker(&mut self, offset: PartitionChunkOffset) {
        if self
            .body_holds
            .get(&offset)
            .is_some_and(|hold| hold.expired)
        {
            return;
        }
        let Some((anchor, _)) = self.covering_range(offset) else {
            return;
        };
        let Some(request) = self.chunk_requests.get_mut(&offset) else {
            return;
        };
        if matches!(request.request_state, ChunkRequestState::HeldByBodyWorker) {
            return;
        }
        request.request_state = ChunkRequestState::HeldByBodyWorker;
        self.body_holds.insert(
            offset,
            BodyHold {
                since: Instant::now(),
                anchor,
                expired: false,
            },
        );
        metrics::record_data_sync_pending_body_skipped();
        #[cfg(test)]
        {
            self.holds_started = self.holds_started.saturating_add(1);
        }
        debug!(
            chunk.offset = %offset,
            "data sync left offset to the body worker"
        );
    }

    fn note_hold_expired(&mut self) {
        metrics::record_data_sync_pending_body_hold_expired();
        #[cfg(test)]
        {
            self.hold_expired = self.hold_expired.saturating_add(1);
        }
    }

    fn is_entropy(&self, offset: PartitionChunkOffset) -> bool {
        matches!(
            self.storage_module.get_chunk_type(&offset),
            Some(ChunkType::Entropy)
        )
    }

    /// Release a hold whose job is gone or whose row key stopped moving.
    ///
    /// The timer is the monotonic time since the hold started. It restarts
    /// when the covering range's start moves forward: that is the pending row
    /// key, so the worker made the prefix durable. Attempt counts do not
    /// restart it. A deferred job never increments them, and that is the case
    /// that must expire. Only an offset that is still Entropy becomes Pending.
    fn release_body_worker_holds(&mut self, now: Instant) {
        let timeout = self.config.data_sync.pending_body_hold_timeout;
        let offsets: Vec<PartitionChunkOffset> = self.chunk_requests.keys().copied().collect();
        let mut promote = Vec::new();
        for offset in offsets {
            let covered = self.covering_range(offset);
            let held = matches!(
                self.chunk_requests[&offset].request_state,
                ChunkRequestState::HeldByBodyWorker
            );
            let hold = self.body_holds.get(&offset).copied();
            match (held, covered, hold) {
                (true, None, _) => {
                    self.body_holds.remove(&offset);
                    if self.is_entropy(offset) {
                        promote.push(offset);
                    }
                }
                (true, Some((start, _)), Some(hold)) if start > hold.anchor => {
                    if let Some(slot) = self.body_holds.get_mut(&offset) {
                        slot.anchor = start;
                        slot.since = now;
                        slot.expired = false;
                    }
                }
                // Already expired, and the key has not moved. Try again. The
                // first expiry is the only one counted. A full queue must not
                // make this offset wait another timeout.
                (true, Some((start, _)), Some(hold)) if hold.expired && start <= hold.anchor => {
                    if self.is_entropy(offset) {
                        promote.push(offset);
                    }
                }
                (true, Some((start, _)), hold)
                    if hold.is_none_or(|hold| {
                        now.saturating_duration_since(hold.since) >= timeout
                    }) =>
                {
                    self.body_holds.insert(
                        offset,
                        BodyHold {
                            since: now,
                            anchor: start,
                            expired: true,
                        },
                    );
                    if self.is_entropy(offset) {
                        promote.push(offset);
                        self.note_hold_expired();
                    }
                }
                (_, None, Some(_)) => {
                    self.body_holds.remove(&offset);
                }
                (_, Some((start, _)), Some(hold)) if hold.expired && start > hold.anchor => {
                    if let Some(slot) = self.body_holds.get_mut(&offset) {
                        slot.anchor = start;
                        slot.since = now;
                        slot.expired = false;
                    }
                }
                _ => {}
            }
        }
        self.body_holds
            .retain(|offset, _| self.chunk_requests.contains_key(offset));
        // Held offsets are not hot. Promoting all of them would skip the
        // pending budget and bury the write head. The rest stay Held and this
        // function takes another batch when a slot is free.
        self.sort_in_scan_order(&mut promote);
        let mut slots = self.free_hot_slots();
        for offset in promote {
            if slots == 0 {
                break;
            }
            if let Some(request) = self.chunk_requests.get_mut(&offset) {
                request.request_state = ChunkRequestState::Pending;
                request.ready_since = now;
                self.ready_offsets.push_back(offset);
                slots -= 1;
            }
        }
    }

    /// Promotes buffered writes the storage module now reports as fsynced, and
    /// returns a wait that has outlived `timeout` to the fetch queue.
    ///
    /// This polls the storage module's interval state rather than consuming a
    /// notification, so nothing can strand a request and the answer is always
    /// the one restart recovery would reload.
    fn reconcile_awaiting_durability(&mut self, now: Instant, timeout: Duration) {
        let mut ready = Vec::new();
        for (offset, request) in &mut self.chunk_requests {
            let ChunkRequestState::AwaitingDurability(started) = request.request_state else {
                continue;
            };

            if self.storage_module.is_data_chunk_durable_at(*offset) {
                request.request_state = ChunkRequestState::Completed;
                metrics::record_data_sync_chunk_stored();
                debug!(
                    chunk.offset = %offset,
                    "data_sync chunk durably stored"
                );
            } else if now.saturating_duration_since(started) >= timeout {
                if now.saturating_duration_since(started) < DURABILITY_PENDING_LIMIT
                    && self.storage_module.is_data_write_pending_at(*offset)
                {
                    continue;
                }
                request.request_state = ChunkRequestState::Pending;
                request.ready_since = now;
                request.excluded.clear();
                ready.push(*offset);
                metrics::record_data_sync_durability_stalled();
                warn!(
                    chunk.offset = %offset,
                    timeout_secs = timeout.as_secs(),
                    "Data-sync write did not become durable in time; returning chunk to pending"
                );
            }
        }
        // A stalled batch shares one fdatasync and returns as a run.
        self.sort_in_scan_order(&mut ready);
        self.ready_offsets.extend(ready);
    }

    /// Order offsets the way the entropy scan visits them: high to low on term
    /// ledgers, low to high on Publish. Requeued neighbours then meet at the
    /// queue head and one cache lookup covers the run, instead of one lookup
    /// per offset in map order.
    fn sort_in_scan_order(&self, offsets: &mut [PartitionChunkOffset]) {
        if self.prioritizes_write_frontier() {
            offsets.sort_unstable_by(|a, b| b.cmp(a));
        } else {
            offsets.sort_unstable();
        }
    }

    #[tracing::instrument(level = "trace", skip_all)]
    fn populate_request_queue(&mut self) {
        self.populate_request_queue_at(Instant::now());
    }

    fn populate_request_queue_at(&mut self, now: Instant) {
        self.reconcile_awaiting_durability(now, DURABILITY_WAIT_TIMEOUT);

        // Retain in-flight requests (for telemetry tracking) and pending entropy requests.
        // Remove completed requests and pending requests for chunks that changed type
        // (satisfied via gossip/upload or invalidated by storage fault/expiry).

        self.chunk_requests.retain(|offset, cr| {
            // Retain network requests and locally buffered writes until their
            // respective completion event arrives.
            if matches!(
                cr.request_state,
                ChunkRequestState::Requested(..) | ChunkRequestState::AwaitingDurability(..)
            ) {
                return true;
            }

            // Drop once the offset is no longer Entropy (stored as Data, or invalidated).
            if !matches!(
                self.storage_module.get_chunk_type(offset),
                Some(ChunkType::Entropy)
            ) {
                return false;
            }

            // Keep Ready / Delayed / Blocked while still Entropy. Drop Completed.
            !matches!(cr.request_state, ChunkRequestState::Completed)
        });

        // A delayed offset becomes ready only after its monotonic deadline.
        // Peer exclusions and the cache-miss memo are cleared here, at the
        // beginning of a new attempt cycle, never in the same transition that
        // handled a 404. The next pop reads the cache once.
        let mut due: Vec<_> = self
            .chunk_requests
            .iter()
            .filter_map(|(&offset, request)| match request.request_state {
                ChunkRequestState::Delayed { retry_at, .. } if retry_at <= now => Some(offset),
                _ => None,
            })
            .collect();
        self.sort_in_scan_order(&mut due);
        // A wave of offsets that 404 on every peer must not take the whole
        // hot budget, or the entropy scan below never runs and never reaches
        // a range a peer can serve. Retries take at most half of the free
        // slots. The rest stay Delayed, already due, and go first next tick.
        due.truncate(self.free_hot_slots().div_ceil(2));
        for offset in due {
            if let Some(request) = self.chunk_requests.get_mut(&offset) {
                request.excluded.clear();
                request.local_cache_miss = false;
                request.request_state = ChunkRequestState::Pending;
                request.ready_since = now;
                self.ready_offsets.push_back(offset);
            }
        }

        // Ready and in-flight work share the configured hot-work budget.
        // Delayed entries do not consume it, otherwise a sparse prefix could
        // prevent discovery of later Entropy forever.
        //
        // Body-worker holds share this discovery budget, so a live job cannot
        // add a fresh budget of rows every tick. `hot_request_count` leaves
        // them out: promotion still has a free slot for a finished hold.
        // While a body job holds the budget, other holes on this module wait
        // until rows release (the anchor moves past them, the job ends, or
        // the hold times out). The term write head is queued from the hot-row
        // count alone, so that wait does not skip the frontier.
        let hot_rows = self.hot_request_count();
        let hot_count = hot_rows.saturating_add(
            self.chunk_requests
                .values()
                .filter(|request| {
                    matches!(request.request_state, ChunkRequestState::HeldByBodyWorker)
                })
                .count(),
        );

        let max_chunk_offset = self.get_max_chunk_offset();
        // `prepare_tick` checks the assignment, then drops that lock to read
        // coverage. A reset can store None before this queue fill. Stop the
        // fill; the requests already queued stay until the module is dropped.
        let Some(pa) = self.storage_module.partition_assignment() else {
            return;
        };

        let Some((max_chunk_offset, ledger_head)) = max_chunk_offset else {
            // Not requests needed
            tracing::trace!(
                "No chunk requests needed for ledger:{:?} slot_index:{:?}",
                pa.ledger_id,
                pa.slot_index
            );
            return;
        };

        // Answers describe one slot. After a move to another slot of the same
        // ledger they say nothing about the new one. Reset before this tick's
        // scan, even while hints are idle: a move repacks the partition.
        if self.hint_slot != pa.slot_index {
            self.assignee_intervals = AssigneeIntervals::new(HINT_FRONTIER_MARGIN);
            self.hint_slot = pa.slot_index;
            self.hints_idle = false;
        }
        let assignees = self.current_assignees();
        if !self.hints_idle {
            let hint_head = self.hint_head(*max_chunk_offset, ledger_head);
            self.plan_assignee_interval_fetches(&assignees, *max_chunk_offset, hint_head, now);
        }
        // An ingress-proof signer in the peer set can serve offsets no assignee
        // holds. Then the scan skips nothing: those offsets are queued, and
        // dispatch sends them to the signer alone.
        let has_signer = self
            .current_peers
            .iter()
            .any(|peer| !assignees.contains(peer));
        let scan_assignees: &[IrysAddress] = if has_signer { &[] } else { &assignees };

        let max_requests = self.config.data_sync.max_pending_chunk_requests as usize;
        let max_offset = *max_chunk_offset;

        // Term ledgers (especially Submit) are a small write head on a packed
        // entropy tail. A low-to-high Entropy walk spends the hot budget on
        // the prefix, so the newest migrated offset is not even queued — the
        // replica looks one chunk short of the frontier. Enqueue that write
        // head first. The service dispatcher also visits term SMs before
        // Publish so a permanent backlog cannot skip this offset; Publish
        // later copies Submit data and needs Submit replicas on the frontier.
        // `get_chunk_type` sees pending writes; the scan below reads the live
        // interval map only.
        //
        // The guard is the non-hold hot count. Holds fill `hot_count` and
        // must not skip this offset.
        let mut head_enqueued = false;
        if hot_rows < max_requests
            && self.prioritizes_write_frontier()
            && self.is_entropy(PartitionChunkOffset::from(max_offset))
            && self.enqueue_pending(PartitionChunkOffset::from(max_offset), now, true)
        {
            head_enqueued = true;
        }

        let mut requests_to_add = max_requests.saturating_sub(hot_count);
        if head_enqueued {
            requests_to_add = requests_to_add.saturating_sub(1);
        }
        // A full hot budget has nothing new to queue. Do not walk entropy intervals.
        if requests_to_add == 0 {
            return;
        }

        // Local Entropy intervals do not distinguish a natural term-ledger gap
        // from a required body that is temporarily unavailable. Keep both
        // unresolved: fair circular discovery plus delayed retries provides
        // eventual reconciliation without falsely marking a 404 synchronized.
        let max_probes = max_requests.saturating_mul(4).max(max_requests);
        let mut probes = 0_usize;
        self.scan_found_entropy = false;
        let mut scan_stopped = false;
        if self.prioritizes_write_frontier() {
            // High offsets near the write head are live data; low-offset Entropy
            // on a term ledger is often an abandoned upload. Walk down from the
            // frontier and only wrap to the prefix after that pass.
            let cursor = self.scan_cursor.min(max_offset);
            let ranges: [(u32, u32); 2] = if cursor == max_offset {
                [(0, max_offset), (1, 0)]
            } else {
                [(0, cursor), (cursor.saturating_add(1), max_offset)]
            };
            'scan: for (range_start, range_end) in ranges {
                if range_start > range_end {
                    continue;
                }
                let budget = max_probes.saturating_sub(probes);
                if budget == 0 {
                    scan_stopped = true;
                    break;
                }
                if self.consume_entropy_scan(
                    scan_assignees,
                    range_start,
                    range_end,
                    true,
                    budget,
                    max_offset,
                    now,
                    &mut probes,
                    max_probes,
                    &mut requests_to_add,
                ) {
                    scan_stopped = true;
                    break 'scan;
                }
            }
        } else {
            // Publish: older (lower) offsets are more likely already replicated
            // on Submit/Publish peers, so fill from the start of the slot.
            self.scan_cursor = self.scan_cursor.min(max_offset);
            let cursor = self.scan_cursor;
            let ranges = [(cursor, max_offset), (0, cursor.saturating_sub(1))];
            'scan: for (range_start, range_end) in ranges {
                if range_start > range_end {
                    continue;
                }
                let budget = max_probes.saturating_sub(probes);
                if budget == 0 {
                    scan_stopped = true;
                    break;
                }
                if self.consume_entropy_scan(
                    scan_assignees,
                    range_start,
                    range_end,
                    false,
                    budget,
                    max_offset,
                    now,
                    &mut probes,
                    max_probes,
                    &mut requests_to_add,
                ) {
                    scan_stopped = true;
                    break 'scan;
                }
            }
        }
        self.hints_idle = !scan_stopped && !self.scan_found_entropy;
    }

    /// Walk one query window. Returns true when this tick must stop.
    ///
    /// Entropy offsets still move the cursor through [`Self::note_scan_offset`].
    /// A window of non-entropy intervals never calls that path, so when the
    /// interval budget is spent the cursor jumps past the window. The wrap
    /// range then waits for the next tick. A chunk-budget clip does not jump:
    /// the returned run is only a prefix of its interval.
    fn consume_entropy_scan(
        &mut self,
        assignees: &[IrysAddress],
        range_start: u32,
        range_end: u32,
        reverse: bool,
        budget: usize,
        max_offset: u32,
        now: Instant,
        probes: &mut usize,
        max_probes: usize,
        requests_to_add: &mut usize,
    ) -> bool {
        let scan = entropy_scan_runs(
            &self.storage_module,
            range_start,
            range_end,
            reverse,
            budget,
        );
        self.scan_found_entropy |= !scan.runs.is_empty();
        let mut stop = false;
        'runs: for (start, end) in scan.runs {
            let mut segments = self.assignee_intervals.segments(assignees, start, end);
            if reverse {
                segments.reverse();
            }
            for (segment_start, segment_end, coverage) in segments {
                if coverage == Coverage::Unclaimed {
                    if self.skip_unclaimed(
                        segment_start,
                        segment_end,
                        max_offset,
                        reverse,
                        probes,
                        max_probes,
                    ) {
                        stop = true;
                        break 'runs;
                    }
                    continue;
                }
                for step in 0..=segment_end - segment_start {
                    let interval_step = if reverse {
                        segment_end - step
                    } else {
                        segment_start + step
                    };
                    if self.note_scan_offset(
                        interval_step,
                        max_offset,
                        reverse,
                        now,
                        probes,
                        max_probes,
                        requests_to_add,
                    ) {
                        stop = true;
                        break 'runs;
                    }
                }
            }
        }
        if stop {
            return true;
        }
        if scan.interval_budget_spent
            && let Some(edge) = scan.visited_edge
        {
            self.scan_cursor = resume_scan_cursor(edge, max_offset, reverse);
            return true;
        }
        false
    }

    /// Move the scan past an Entropy span every assignee answered without,
    /// queueing none of it. Each skipped offset counts as a probe, so a tick
    /// visits no more of the map than it would while probing.
    fn skip_unclaimed(
        &mut self,
        start: u32,
        end: u32,
        max_offset: u32,
        reverse: bool,
        probes: &mut usize,
        max_probes: usize,
    ) -> bool {
        let span = usize::try_from(end - start)
            .unwrap_or(usize::MAX)
            .saturating_add(1);
        *probes = probes.saturating_add(span);
        self.scan_cursor =
            resume_scan_cursor(if reverse { start } else { end }, max_offset, reverse);
        metrics::record_data_sync_hint_skipped(self.ledger_id, span as u64);
        *probes >= max_probes
    }

    /// The write head as interval hints see it. Hints stop
    /// [`HINT_FRONTIER_MARGIN`] below it, because a peer's map lags chunks
    /// that are still migrating. A partition the ledger has filled has no such
    /// chunks, so its hints reach its last offset.
    fn hint_head(&self, max_offset: u32, ledger_head: LedgerChunkOffset) -> u32 {
        let head_inside = self
            .storage_module
            .get_storage_module_ledger_offsets()
            .ok()
            .is_none_or(|range| range.end() > ledger_head);
        if head_inside {
            max_offset
        } else {
            max_offset.saturating_add(HINT_FRONTIER_MARGIN)
        }
    }

    /// Peers in the current set that are assigned this ledger slot. Only their
    /// interval answers decide what the scan may skip.
    fn current_assignees(&self) -> Vec<IrysAddress> {
        let slot_index = self
            .storage_module
            .partition_assignment()
            .and_then(|pa| pa.slot_index);
        let Ok(peers) = self.active_sync_peers.read() else {
            return Vec::new();
        };
        self.current_peers
            .iter()
            .copied()
            .filter(|address| {
                peers
                    .get(address)
                    .is_some_and(|peer| peer.is_assigned_to(self.ledger_id, slot_index))
            })
            .collect()
    }

    /// Start the interval fetches that are due. Each answer comes back as
    /// [`DataSyncServiceMessage::AssigneeIntervalsFetched`].
    fn plan_assignee_interval_fetches(
        &mut self,
        assignees: &[IrysAddress],
        max_offset: u32,
        hint_head: u32,
        now: Instant,
    ) {
        let Some(slot_index) = self
            .storage_module
            .partition_assignment()
            .and_then(|pa| pa.slot_index)
        else {
            return;
        };
        let reverse = self.prioritizes_write_frontier();
        let planned = self.assignee_intervals.plan_fetches(
            assignees,
            self.scan_cursor,
            max_offset,
            reverse,
            now,
        );
        for fetch in planned {
            let api = self
                .active_sync_peers
                .read()
                .ok()
                .and_then(|peers| peers.get(&fetch.peer).map(|peer| peer.peer_address.api));
            let Some(api) = api else {
                self.assignee_intervals.on_fetched(
                    fetch,
                    hint_head,
                    Err(ChunkFetchError::NetworkError {
                        message: "peer left the peer set".to_string(),
                    }),
                    now,
                );
                continue;
            };
            let fetcher = Arc::clone(&self.chunk_fetcher);
            let tx = self.service_senders.data_sync.clone();
            let storage_module_id = self.storage_module.id;
            self.runtime_handle.spawn(
                async move {
                    let result = assignee_intervals::fetch_window(
                        &*fetcher,
                        slot_index,
                        fetch.start,
                        fetch.end,
                        api,
                        HINT_PAGE_TIMEOUT,
                    )
                    .await;
                    let _ = tx.send_traced(DataSyncServiceMessage::AssigneeIntervalsFetched {
                        storage_module_id,
                        fetch,
                        head: hint_head,
                        result,
                    });
                }
                .instrument(tracing::Span::current()),
            );
        }
    }

    /// An assignee's interval answer arrived.
    pub fn on_assignee_intervals(
        &mut self,
        fetch: WindowFetch,
        head: u32,
        result: Result<DataIntervalsPage, ChunkFetchError>,
    ) {
        match &result {
            Ok(page) => debug!(
                peer.address = %fetch.peer,
                window.start = fetch.start,
                window.end = fetch.end,
                intervals = page.intervals.len(),
                coverage = ?page.coverage,
                "data sync assignee intervals"
            ),
            Err(error) => debug!(
                peer.address = %fetch.peer,
                window.start = fetch.start,
                window.end = fetch.end,
                error = %error,
                "data sync assignee intervals unavailable; probing per offset"
            ),
        }
        self.assignee_intervals
            .on_fetched(fetch, head, result, Instant::now());
    }

    /// Submit / OneYear / ThirtyDay: prefer the migrated write head. Publish
    /// keeps the existing low-to-high walk so a large permanent backlog still
    /// fills from the start of the slot. The service dispatcher also visits
    /// these orchestrators before Publish so Submit replicas stay on the
    /// frontier and Publish has many sources to copy from.
    pub(crate) fn prioritizes_write_frontier(&self) -> bool {
        Self::ledger_prioritizes_write_frontier(self.ledger_id)
    }

    fn ledger_prioritizes_write_frontier(ledger_id: u32) -> bool {
        matches!(
            DataLedger::try_from(ledger_id),
            Ok(DataLedger::Submit | DataLedger::OneYear | DataLedger::ThirtyDay)
        )
    }

    /// Advance `scan_cursor` past `interval_step` and maybe enqueue it.
    /// Returns true when the scan should stop (budget or probe cap).
    fn note_scan_offset(
        &mut self,
        interval_step: u32,
        max_offset: u32,
        reverse: bool,
        now: Instant,
        probes: &mut usize,
        max_probes: usize,
        requests_to_add: &mut usize,
    ) -> bool {
        *probes += 1;
        self.scan_cursor = if reverse {
            if interval_step == 0 {
                u32::MAX
            } else {
                interval_step - 1
            }
        } else if interval_step == max_offset {
            0
        } else {
            interval_step.saturating_add(1)
        };
        // The scan reads the live interval map. Pending Data is still Entropy
        // there until the write is synced, so confirm the point before enqueue.
        // The probe is counted either way and the cursor has moved on.
        if self.is_entropy(PartitionChunkOffset::from(interval_step))
            && self.enqueue_pending(PartitionChunkOffset::from(interval_step), now, false)
        {
            *requests_to_add = requests_to_add.saturating_sub(1);
            if *requests_to_add == 0 {
                return true;
            }
        }
        *probes >= max_probes
    }

    /// Insert `offset` as Pending if it is not already tracked. `at_front`
    /// places it at the dispatch head (term-ledger write head); otherwise it
    /// joins the FIFO tail.
    fn enqueue_pending(
        &mut self,
        chunk_offset: PartitionChunkOffset,
        now: Instant,
        at_front: bool,
    ) -> bool {
        let hash_map::Entry::Vacant(entry) = self.chunk_requests.entry(chunk_offset) else {
            return false;
        };
        entry.insert(ChunkRequest {
            excluded: HashSet::new(),
            request_state: ChunkRequestState::Pending,
            attempt_count: 0,
            retry_round: 0,
            ready_since: now,
            local_cache_miss: false,
            body_in_ingress: false,
        });
        if at_front {
            self.ready_offsets.push_front(chunk_offset);
        } else {
            self.ready_offsets.push_back(chunk_offset);
        }
        true
    }

    /// Dispatch at most one ready offset. Returning `false` lets the service
    /// stop the current round when this storage module cannot make progress.
    #[tracing::instrument(skip_all)]
    pub fn dispatch_next(&mut self) -> bool {
        if self.should_throttle_requests() {
            debug!("Throttling chunk requests due to storage throughput");
            return false;
        }

        if !self.coverage_loaded {
            self.reload_pending_body_coverage();
        }

        // Offsets this call may set aside because no peer they may use has a
        // free slot. Bounded by the queue so a call cannot spin.
        let mut set_aside_budget = self.ready_offsets.len();
        while let Some(chunk_offset) = self.ready_offsets.pop_front() {
            // An assignee that answered without this offset is not asked for it.
            let absent: Vec<IrysAddress> = self
                .current_peers
                .iter()
                .copied()
                .filter(|peer| self.assignee_intervals.excludes(peer, chunk_offset.0))
                .collect();
            let Some((excluded, all_peers_exhausted, cache_miss)) = self
                .chunk_requests
                .get_mut(&chunk_offset)
                .and_then(|chunk_request| {
                    if !matches!(chunk_request.request_state, ChunkRequestState::Pending) {
                        return None;
                    }
                    // Peer churn must not let exclusion bookkeeping grow without bound.
                    chunk_request
                        .excluded
                        .retain(|peer| self.current_peers.contains(peer));
                    let exhausted = !self.current_peers.is_empty()
                        && self.current_peers.iter().all(|peer| {
                            chunk_request.excluded.contains(peer) || absent.contains(peer)
                        });
                    let mut excluded = chunk_request.excluded.clone();
                    excluded.extend(absent.iter().copied());
                    Some((excluded, exhausted, chunk_request.local_cache_miss))
                })
            else {
                continue;
            };

            // A live body job owns this offset. Hold it before the cache read
            // so a miss is not memoized and the disk lane is not raced. An
            // expired hold falls through to the cache and to peers.
            if self.body_worker_owns(chunk_offset) {
                self.hold_for_body_worker(chunk_offset);
                continue;
            }

            // The cache is tried before peer selection. A node that already
            // holds the body must write it even when every peer is exhausted
            // or none is connected. A recorded miss skips this until the
            // request is dropped.
            if !cache_miss {
                // Index read, cache read, and hash run on this actor. Stop the
                // tick at the cap and put the offset back. The next tick starts
                // the count at zero. A cap of zero is one lookup, so the offset
                // cannot sit at the front forever.
                if self.cache_lookups_this_tick >= self.local_cache_lookup_limit() {
                    self.ready_offsets.push_front(chunk_offset);
                    return false;
                }
                self.cache_lookups_this_tick = self.cache_lookups_this_tick.saturating_add(1);
                match self.try_local_cache(chunk_offset) {
                    LocalCacheStep::Done => return true,
                    LocalCacheStep::Stop => return false,
                    LocalCacheStep::Miss => {}
                }
            }

            if all_peers_exhausted {
                self.delay_request(chunk_offset, Instant::now());
                continue;
            }

            let Some(peer_address) = self.find_best_peer(Some(&excluded)) else {
                if set_aside_budget == 0 || self.find_best_peer(None).is_none() {
                    // No peer has a free slot. Keep this offset's place at the
                    // head; the next released slot or tick takes it.
                    self.ready_offsets.push_front(chunk_offset);
                    return false;
                }
                // A peer has a free slot, but not one this offset may use, for
                // example only a busy signer can serve it. Later offsets may use
                // that slot. This one waits at the tail.
                set_aside_budget -= 1;
                self.ready_offsets.push_back(chunk_offset);
                continue;
            };

            match self.dispatch_chunk_request(chunk_offset, peer_address) {
                DispatchStart::Started => return true,
                DispatchStart::Skip => {}
                // The offset was put back and the same peer is still
                // selectable. Another pop this round would spin.
                DispatchStart::Stop => return false,
            }
        }

        false
    }

    /// Lookups allowed in this tick. At least one, so a zero config still
    /// makes progress instead of requeuing the same offset every pop.
    fn local_cache_lookup_limit(&self) -> u32 {
        u32::try_from(self.config.data_sync.max_local_cache_hits_per_tick)
            .unwrap_or(u32::MAX)
            .max(1)
    }

    /// How many contiguous offsets one local-cache read may cover.
    /// Matches the ingress sweep so one enqueue is one disk span.
    fn local_cache_run_limit(&self) -> usize {
        let chunk = self.config.consensus_config().chunk_size.max(1);
        let sweep = self.config.storage.entropy_sweep_max_bytes.max(chunk);
        usize::try_from(sweep / chunk).unwrap_or(1).max(1)
    }

    /// `start` is already popped and Pending. Append following Pending offsets
    /// that continue the run, are not cache-miss memoized, and sit at the queue
    /// head. Stale non-pending heads are dropped, same as [`Self::dispatch_next`].
    ///
    /// Term ledgers queue offsets high to low, so a run may descend. The first
    /// appended offset fixes the direction for the rest of the run.
    fn take_local_cache_run(&mut self, start: PartitionChunkOffset) -> Vec<PartitionChunkOffset> {
        let mut run = vec![start];
        let mut descending = None;
        let limit = self.local_cache_run_limit();
        while run.len() < limit {
            let Some(last) = run.last().copied() else {
                break;
            };
            let Some(&front) = self.ready_offsets.front() else {
                break;
            };
            let (pending, cache_miss) = {
                let Some(request) = self.chunk_requests.get(&front) else {
                    self.ready_offsets.pop_front();
                    continue;
                };
                (
                    matches!(request.request_state, ChunkRequestState::Pending),
                    request.local_cache_miss,
                )
            };
            if !pending {
                self.ready_offsets.pop_front();
                continue;
            }
            let up = last.0.checked_add(1) == Some(front.0);
            let down = last.0.checked_sub(1) == Some(front.0);
            let continues = match descending {
                None => up || down,
                Some(true) => down,
                Some(false) => up,
            };
            // Stop before an offset a body job still owns. The next pop holds it.
            if !continues || cache_miss || self.body_worker_owns(front) {
                break;
            }
            descending.get_or_insert(down);
            self.ready_offsets.pop_front();
            run.push(front);
        }
        run
    }

    fn push_front_offsets(&mut self, offsets: &[PartitionChunkOffset]) {
        for offset in offsets.iter().rev() {
            self.ready_offsets.push_front(*offset);
        }
    }

    fn memo_local_cache_miss(&mut self, offset: PartitionChunkOffset) {
        if let Some(request) = self.chunk_requests.get_mut(&offset) {
            request.local_cache_miss = true;
        }
    }

    /// Offsets in Pending, Requested, or AwaitingDurability.
    ///
    /// Held, Delayed, and Blocked stay out of this count. Promotion uses it,
    /// so a finished hold can still become Pending. Discovery adds the hold
    /// count on top.
    fn hot_request_count(&self) -> usize {
        self.chunk_requests
            .values()
            .filter(|request| {
                matches!(
                    request.request_state,
                    ChunkRequestState::Pending
                        | ChunkRequestState::Requested(..)
                        | ChunkRequestState::AwaitingDurability(..)
                )
            })
            .count()
    }

    fn free_hot_slots(&self) -> usize {
        (self.config.data_sync.max_pending_chunk_requests as usize)
            .saturating_sub(self.hot_request_count())
    }

    /// Queue entropy holes from a retired body-migration job at the tail.
    ///
    /// Clears a local-cache miss memo so a later dispatch reads the cache once
    /// more. An in-flight fetch is left alone. A blocked offset keeps its
    /// reason. New holes are inserted only while a hot slot is free; the hole
    /// scan picks up the rest. An offset already in the ready queue keeps its
    /// place, so this does not pass the term-ledger write head.
    pub(crate) fn queue_body_migration_holes(&mut self, offsets: &[PartitionChunkOffset]) {
        let now = Instant::now();
        let mut slots = self.free_hot_slots();
        let mut queued = Vec::new();
        for &offset in offsets {
            if !matches!(
                self.storage_module.get_chunk_type(&offset),
                Some(ChunkType::Entropy)
            ) {
                continue;
            }
            match self.chunk_requests.get_mut(&offset) {
                Some(request)
                    if matches!(
                        request.request_state,
                        ChunkRequestState::Requested(_, _)
                            | ChunkRequestState::AwaitingDurability(_)
                    ) =>
                {
                    continue;
                }
                Some(request) if matches!(request.request_state, ChunkRequestState::Blocked(_)) => {
                    request.local_cache_miss = false;
                }
                Some(request) => {
                    request.local_cache_miss = false;
                    if !matches!(request.request_state, ChunkRequestState::Pending) {
                        if slots == 0 {
                            continue;
                        }
                        request.request_state = ChunkRequestState::Pending;
                        request.ready_since = now;
                        slots -= 1;
                    }
                    queued.push(offset);
                }
                None => {
                    if slots == 0 {
                        continue;
                    }
                    if self.enqueue_pending(offset, now, false) {
                        slots -= 1;
                        queued.push(offset);
                    }
                }
            }
        }
        if queued.is_empty() {
            return;
        }
        let already: HashSet<_> = self.ready_offsets.iter().copied().collect();
        for offset in queued {
            if !already.contains(&offset) {
                self.ready_offsets.push_back(offset);
            }
        }
    }

    /// Put `offset` back on the FIFO tail without recording a cache miss.
    /// A write that did not land can succeed on a later tick.
    fn requeue_pending_without_memo(&mut self, offset: PartitionChunkOffset) {
        let Some(request) = self.chunk_requests.get_mut(&offset) else {
            return;
        };
        request.request_state = ChunkRequestState::Pending;
        request.ready_since = Instant::now();
        self.ready_offsets.push_back(offset);
    }

    fn note_local_cache_error(
        &mut self,
        start: PartitionChunkOffset,
        tail: &[PartitionChunkOffset],
    ) {
        metrics::record_data_sync_fetch_by_source("local_cache", "error");
        self.push_front_offsets(tail);
        debug!(
            chunk.offset = %start,
            "local cache lookup failed; falling through to peers"
        );
    }

    /// Read the main-db chunk cache for `start` and a contiguous Pending run.
    ///
    /// Hits skip ingress (the leaf is already committed) and go to the disk
    /// lane. Misses, including an index row with no body, are memoized.
    /// [`LocalCacheStep::Stop`] means the start offset was requeued after a
    /// write failure so this tick does not spin.
    fn try_local_cache(&mut self, start: PartitionChunkOffset) -> LocalCacheStep {
        let run = self.take_local_cache_run(start);
        let Some(last) = run.last().copied() else {
            return LocalCacheStep::Miss;
        };
        let placements = irys_domain::trace_index_read(irys_domain::DATA_SYNC, || {
            self.storage_module
                .data_root_and_tx_offset_span(start.min(last), start.max(last))
        });
        let placements = match placements {
            Ok(mut placements) if placements.len() == run.len() => {
                // The span reads low to high. Every later step indexes by run
                // position, so a descending run needs its placements reversed.
                if last < start {
                    placements.reverse();
                }
                placements
            }
            Ok(_) => {
                warn!(
                    chunk.offset = %start,
                    "local cache placement span length mismatch"
                );
                self.note_local_cache_error(start, &run[1..]);
                return LocalCacheStep::Miss;
            }
            Err(error) => {
                warn!(
                    chunk.offset = %start,
                    error = %error,
                    "local cache placement read failed"
                );
                self.note_local_cache_error(start, &run[1..]);
                return LocalCacheStep::Miss;
            }
        };

        let looked_up = self.db.view_eyre(|tx| {
            let mut rows = Vec::with_capacity(placements.len());
            for placement in &placements {
                let row = match placement {
                    Some((data_commitment, _, tx_offset, _)) => {
                        cached_chunk_by_chunk_offset(tx, *data_commitment, *tx_offset)?
                            .map(|(_, chunk)| chunk)
                    }
                    None => None,
                };
                rows.push(row);
            }
            Ok(rows)
        });
        let looked_up = match looked_up {
            Ok(rows) if rows.len() == run.len() => rows,
            Ok(_) => {
                warn!(chunk.offset = %start, "local cache row count mismatch");
                self.note_local_cache_error(start, &run[1..]);
                return LocalCacheStep::Miss;
            }
            Err(error) => {
                warn!(
                    chunk.offset = %start,
                    error = %error,
                    "local cache body read failed"
                );
                self.note_local_cache_error(start, &run[1..]);
                return LocalCacheStep::Miss;
            }
        };

        let Ok(geometry) = self.config.consensus_config().data_tree_geometry() else {
            warn!(chunk.offset = %start, "configured data tree geometry is invalid");
            self.note_local_cache_error(start, &run[1..]);
            return LocalCacheStep::Miss;
        };

        let mut hits = Vec::new();
        let mut tail_misses = Vec::new();
        for (index, offset) in run.iter().copied().enumerate() {
            let miss = match (&placements[index], &looked_up[index]) {
                (Some((_, data_root, tx_offset, data_size)), Some(cached))
                    if cached.chunk.is_some() =>
                {
                    match validate_chunk_for_migration(
                        cached.clone(),
                        *data_root,
                        *data_size,
                        *tx_offset,
                        &geometry,
                    ) {
                        Ok(unpacked) => {
                            hits.push((offset, unpacked));
                            None
                        }
                        Err(error) => {
                            debug!(
                                chunk.offset = %offset,
                                error = %error,
                                "local cache chunk failed migration checks"
                            );
                            metrics::record_data_sync_fetch_by_source("local_cache", "invalid");
                            Some(())
                        }
                    }
                }
                _ => {
                    metrics::record_data_sync_fetch_by_source("local_cache", "not_found");
                    Some(())
                }
            };
            if miss.is_some() {
                self.memo_local_cache_miss(offset);
                if index > 0 {
                    tail_misses.push(offset);
                }
            }
        }

        debug!(
            chunk.offset = %start,
            local_cache.run = run.len(),
            local_cache.hits = hits.len(),
            "data sync local cache lookup"
        );

        let start_was_hit = hits.iter().any(|(offset, _)| *offset == start);
        match self.publish_cached_bodies(&hits) {
            Err(()) => {
                self.push_front_offsets(&tail_misses);
                if start_was_hit {
                    LocalCacheStep::Stop
                } else {
                    LocalCacheStep::Miss
                }
            }
            Ok(accepted) => {
                self.push_front_offsets(&tail_misses);
                if accepted.contains(&start) {
                    LocalCacheStep::Done
                } else if start_was_hit {
                    LocalCacheStep::Stop
                } else {
                    LocalCacheStep::Miss
                }
            }
        }
    }

    /// Enqueue validated cache bodies. `Err` means the whole batch was refused
    /// and every hit was requeued. `Ok` is the set of offsets that left `Pending`.
    fn publish_cached_bodies(
        &mut self,
        hits: &[(PartitionChunkOffset, UnpackedChunk)],
    ) -> Result<HashSet<PartitionChunkOffset>, ()> {
        if hits.is_empty() {
            return Ok(HashSet::new());
        }
        let chunks: Vec<UnpackedChunk> = hits.iter().map(|(_, chunk)| chunk.clone()).collect();
        let items = match self.storage_module.enqueue_ingress_batch(&chunks) {
            Ok(items) => items,
            Err(error) => {
                warn!(error = %error, "local cache ingress enqueue failed");
                let reason = write_error_label(&error);
                for (offset, _) in hits {
                    self.requeue_pending_without_memo(*offset);
                    metrics::record_data_sync_fetch_by_source("local_cache", "write_failed");
                    metrics::record_data_sync_chunk_write_failed(reason);
                }
                return Err(());
            }
        };

        let mut accepted = HashSet::new();
        for ((offset, chunk), item) in hits.iter().zip(items.iter()) {
            let outcome = super::classify_synced_chunk(&self.storage_module, chunk, *offset, item);
            if self.apply_local_cache_write(*offset, &outcome) {
                accepted.insert(*offset);
            } else {
                let reason = match &outcome {
                    super::DataSyncWriteOutcome::NoWriteableOffset => "no_writeable_offset",
                    super::DataSyncWriteOutcome::Other(_) => "other",
                    super::DataSyncWriteOutcome::AlreadyDurable
                    | super::DataSyncWriteOutcome::AwaitingDurability
                    | super::DataSyncWriteOutcome::MissingDataRootIndex => "other",
                };
                self.requeue_pending_without_memo(*offset);
                metrics::record_data_sync_fetch_by_source("local_cache", "write_failed");
                metrics::record_data_sync_chunk_write_failed(reason);
            }
        }
        for (offset, _) in hits.iter().skip(items.len()) {
            self.requeue_pending_without_memo(*offset);
            metrics::record_data_sync_fetch_by_source("local_cache", "write_failed");
            metrics::record_data_sync_chunk_write_failed("other");
        }
        Ok(accepted)
    }

    /// Returns true when `offset` is no longer Pending.
    fn apply_local_cache_write(
        &mut self,
        offset: PartitionChunkOffset,
        outcome: &super::DataSyncWriteOutcome,
    ) -> bool {
        let applied = {
            let Some(request) = self.chunk_requests.get_mut(&offset) else {
                return false;
            };
            match outcome {
                super::DataSyncWriteOutcome::AlreadyDurable => {
                    request.request_state = ChunkRequestState::Completed;
                    metrics::record_data_sync_chunk_stored();
                    metrics::record_data_sync_fetch_by_source("local_cache", "success");
                    true
                }
                super::DataSyncWriteOutcome::AwaitingDurability => {
                    request.request_state = ChunkRequestState::AwaitingDurability(Instant::now());
                    metrics::record_data_sync_fetch_by_source("local_cache", "success");
                    true
                }
                super::DataSyncWriteOutcome::MissingDataRootIndex => {
                    request.request_state =
                        ChunkRequestState::Blocked(ChunkBlockReason::MissingDataRootIndex);
                    request.excluded.clear();
                    metrics::record_data_sync_chunk_blocked(
                        ChunkBlockReason::MissingDataRootIndex.as_metric_label(),
                    );
                    metrics::record_data_sync_fetch_by_source("local_cache", "write_failed");
                    true
                }
                super::DataSyncWriteOutcome::NoWriteableOffset
                | super::DataSyncWriteOutcome::Other(_) => false,
            }
        };
        if applied && matches!(outcome, super::DataSyncWriteOutcome::MissingDataRootIndex) {
            self.trim_blocked_requests();
        }
        applied
    }

    /// `jitter_bucket` is the local-cache run length. Offsets in one aligned
    /// bucket share a deadline, so a contiguous hole comes due in one tick and
    /// its retries still form one cache run. Different buckets spread out.
    fn retry_delay(
        chunk_offset: PartitionChunkOffset,
        retry_round: u32,
        jitter_bucket: u64,
    ) -> Duration {
        let exponent = retry_round.saturating_sub(1).min(6);
        let base = RETRY_BACKOFF_INITIAL
            .saturating_mul(1_u32 << exponent)
            .min(RETRY_BACKOFF_MAX);
        // Stable per-bucket jitter avoids synchronized retry waves without a
        // random generator or wall-clock dependency. Keep the capped maximum.
        let jitter_window_ms = (base.as_millis() / 4) as u64;
        let bucket = u64::from(*chunk_offset) / jitter_bucket.max(1);
        let jitter_ms = if jitter_window_ms == 0 {
            0
        } else {
            bucket.wrapping_mul(0x9e37_79b9) % (jitter_window_ms + 1)
        };
        // At the cap, open the window below the maximum. Clamping after the
        // add would put every capped retry on the same deadline, so a full
        // delayed set would return as one wave and fill the hot budget.
        let base =
            base.min(RETRY_BACKOFF_MAX.saturating_sub(Duration::from_millis(jitter_window_ms)));
        base.saturating_add(Duration::from_millis(jitter_ms))
            .min(RETRY_BACKOFF_MAX)
    }

    fn delay_request(&mut self, chunk_offset: PartitionChunkOffset, now: Instant) {
        let jitter_bucket = u64::try_from(self.local_cache_run_limit()).unwrap_or(1);
        let Some(request) = self.chunk_requests.get_mut(&chunk_offset) else {
            return;
        };
        request.retry_round = request.retry_round.saturating_add(1);
        let retry_round = request.retry_round;
        let delay = Self::retry_delay(chunk_offset, retry_round, jitter_bucket);
        request.request_state = ChunkRequestState::Delayed {
            retry_at: now + delay,
        };
        metrics::record_data_sync_retry_delay(self.ledger_id, delay);
        metrics::record_data_sync_peers_exhausted(self.ledger_id);
        self.trim_delayed_requests();
    }

    fn trim_blocked_requests(&mut self) {
        let limit = (self.config.data_sync.max_pending_chunk_requests as usize)
            .saturating_mul(BLOCKED_REQUEST_LIMIT_MULTIPLIER)
            .max(1);
        let protected = if self.prioritizes_write_frontier() {
            self.get_max_chunk_offset().map(|(offset, _)| offset)
        } else {
            None
        };
        let mut blocked_count = self
            .chunk_requests
            .values()
            .filter(|request| matches!(request.request_state, ChunkRequestState::Blocked(_)))
            .count();
        while blocked_count > limit {
            let Some(offset) = self
                .chunk_requests
                .iter()
                .filter_map(|(&offset, request)| {
                    matches!(request.request_state, ChunkRequestState::Blocked(_)).then_some(offset)
                })
                .filter(|offset| Some(*offset) != protected)
                .max()
            else {
                break;
            };
            self.chunk_requests.remove(&offset);
            blocked_count -= 1;
        }
    }

    fn trim_delayed_requests(&mut self) {
        let limit = (self.config.data_sync.max_pending_chunk_requests as usize)
            .saturating_mul(DELAYED_REQUEST_LIMIT_MULTIPLIER)
            .max(1);
        let mut delayed_count = self
            .chunk_requests
            .values()
            .filter(|request| matches!(request.request_state, ChunkRequestState::Delayed { .. }))
            .count();
        while delayed_count > limit {
            let Some(offset) = self
                .chunk_requests
                .iter()
                .filter_map(|(&offset, request)| match request.request_state {
                    ChunkRequestState::Delayed { retry_at, .. } => Some((retry_at, offset)),
                    _ => None,
                })
                .min_by_key(|(retry_at, _)| *retry_at)
                .map(|(_, offset)| offset)
            else {
                break;
            };
            self.chunk_requests.remove(&offset);
            delayed_count -= 1;
        }
    }

    #[tracing::instrument(level = "trace", skip_all)]
    fn should_throttle_requests(&self) -> bool {
        let storage_throughput = self.storage_module.write_throughput_bps();
        let target_throughput = self.config.data_sync.max_storage_throughput_bps;
        let storage_capacity_remaining = target_throughput.saturating_sub(storage_throughput);

        let should_throttle = storage_capacity_remaining < (target_throughput / 10);

        debug!(
            "Throttle check: throughput={} target={} remaining={} throttle={}",
            storage_throughput, target_throughput, storage_capacity_remaining, should_throttle
        );

        // If we're within 10% of target_throughput, throttle this orchestrator
        should_throttle
    }

    #[tracing::instrument(skip_all)]
    pub fn get_max_chunk_offset(&self) -> Option<(PartitionChunkOffset, LedgerChunkOffset)> {
        // Find the maximum LedgerRelativeOffset of this storage module
        // The assignment can be cleared before SyncPartitions drops this
        // orchestrator: nothing to sync then.
        let ledger_range = self
            .storage_module
            .get_storage_module_ledger_offsets()
            .ok()?;

        // Fetch the most recently migrated block
        // We only want to download migrated chunks from other peers
        let max_chunk_offset: Option<u64> = {
            let tree = self.block_tree.read();
            let (canonical, _) = tree.get_canonical_chain();
            let block_migration_depth =
                self.config.consensus_config().block_migration_depth as usize;

            if canonical.len() >= block_migration_depth {
                let most_recent_migrated_block =
                    &canonical[canonical.len() - block_migration_depth];

                let block = most_recent_migrated_block.header();

                match tree
                    .consensus_config()
                    .hardforks
                    .classify_data_ledger(block, self.ledger_id)
                {
                    DataLedgerLookup::Present(dl) if dl.total_chunks == 0 => None,
                    DataLedgerLookup::Present(dl) => Some(dl.total_chunks.saturating_sub(1)),
                    // A migrated block that predates this ledger's activation
                    // (e.g. a pre-Cascade block for the OneYear/ThirtyDay term
                    // ledgers) legitimately has no entry for it — nothing to
                    // sync for this ledger yet.
                    DataLedgerLookup::ExpectedAbsent => {
                        debug!(
                            ledger_id = self.ledger_id,
                            block_height = block.height,
                            "migrated block predates this ledger's activation; nothing to sync yet"
                        );
                        None
                    }
                    // The block's shape is validated upstream, so this should be
                    // unreachable; surface it (defense-in-depth) but still degrade
                    // to None rather than aborting the task.
                    DataLedgerLookup::UnexpectedAbsent => {
                        warn!(
                            ledger_id = self.ledger_id,
                            block_height = block.height,
                            "data ledger missing from migrated block where consensus expects it; nothing to sync"
                        );
                        None
                    }
                }
            } else {
                None
            }
        };

        // If we couldn't find a valid max_chunk_offset return None
        let max_chunk_offset = max_chunk_offset?;

        // is the max chunk offset before the start of this storage module (can happen at head of chain)
        if ledger_range.start() > max_chunk_offset.into() {
            // Ledger range of the partition starts after the max_chunk_offset meaning don't attempt to sync anything
            return None;
        }

        if ledger_range.end() > max_chunk_offset.into() {
            let part_relative: u64 = max_chunk_offset.saturating_sub(ledger_range.start().into());
            Some((
                PartitionChunkOffset::from(part_relative as u32),
                LedgerChunkOffset::from(max_chunk_offset),
            ))
        } else {
            // Otherwise just return the maximum PartitionChunkOffset
            let max = ledger_range.end() - ledger_range.start();
            Some((
                PartitionChunkOffset::from(max),
                LedgerChunkOffset::from(max_chunk_offset),
            ))
        }
    }

    fn find_best_peer(&self, excluding: Option<&ExcludedPeerAddresses>) -> Option<IrysAddress> {
        let peers = self.active_sync_peers.read().ok()?;
        let slot_index = self
            .storage_module
            .partition_assignment()
            .and_then(|pa| pa.slot_index);

        let mut candidates: Vec<&PeerBandwidthManager> = self
            .current_peers
            .iter()
            .filter_map(|&addr| peers.get(&addr))
            .filter(|peer_manager| {
                peer_manager.available_concurrency() > 0
                    && match &excluding {
                        Some(excluded) => !excluded.contains(&peer_manager.miner_address),
                        None => true,
                    }
            })
            .collect();

        if candidates.is_empty() {
            return None;
        }

        // Prefer assigned replicas for the common path; promote ingress-proof
        // signers (no matching ledger/slot assignment) only after assignees are
        // exhausted or excluded (e.g. residual empty×empty 404 loop).
        // Then: health score, then available concurrency.
        candidates.sort_by(|a, b| {
            let a_assigned = a.is_assigned_to(self.ledger_id, slot_index);
            let b_assigned = b.is_assigned_to(self.ledger_id, slot_index);
            (b_assigned, b.health_score(), b.available_concurrency())
                .partial_cmp(&(a_assigned, a.health_score(), a.available_concurrency()))
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        candidates
            .first()
            .map(|peer_manager| peer_manager.miner_address)
    }

    #[tracing::instrument(skip_all)]
    fn dispatch_chunk_request(
        &mut self,
        chunk_offset: PartitionChunkOffset,
        peer_addr: IrysAddress,
    ) -> DispatchStart {
        let Some(request) = self.chunk_requests.get_mut(&chunk_offset) else {
            return DispatchStart::Skip;
        };

        let slot_index = self
            .storage_module
            .partition_assignment()
            .and_then(|pa| pa.slot_index);

        let (api_addr, use_ledger_offset) = {
            let peers = match self.active_sync_peers.read() {
                Ok(p) => p,
                Err(_) => {
                    self.ready_offsets.push_back(chunk_offset);
                    return DispatchStart::Skip;
                }
            };
            let Some(peer_manager) = peers.get(&peer_addr) else {
                // The peer can disappear between selection and dispatch. Keep
                // the still-Pending offset in the FIFO for another peer.
                self.ready_offsets.push_back(chunk_offset);
                return DispatchStart::Skip;
            };
            (
                peer_manager.peer_address.api,
                peer_manager.is_assigned_to(self.ledger_id, slot_index),
            )
        };

        // Dual addressing:
        // - Assignees → ledger-offset (existing)
        // - Ingress-proof signers (may lack SM) → data_root + tx_offset
        // Resolve mode *before* accounting a started request so we do not
        // penalize proof signers when we cannot map the residual hole.
        let fetch_mode = if use_ledger_offset {
            let Ok(range) = self.storage_module.get_storage_module_ledger_offsets() else {
                // The local assignment can clear while the peer map lock was
                // held. The peer is still selectable, so continuing this round
                // would pop the same offset again. Leave it Pending.
                self.ready_offsets.push_back(chunk_offset);
                return DispatchStart::Stop;
            };
            let start_ledger_offset = u64::from(range.start());
            let ledger_chunk_offset =
                LedgerChunkOffset::from(start_ledger_offset + u64::from(chunk_offset));
            FetchMode::LedgerOffset(ledger_chunk_offset)
        } else {
            // The placement's commitment names the request, so the peer serves
            // the chunk of this commitment and not another size of the root.
            match irys_domain::trace_index_read(irys_domain::DATA_SYNC, || {
                self.storage_module
                    .data_root_and_tx_offset_span(chunk_offset, chunk_offset)
                    .map(|span| span.into_iter().next().flatten())
            }) {
                Ok(Some((data_commitment, _, tx_offset, _))) => FetchMode::DataCommitment {
                    data_commitment,
                    tx_offset,
                },
                Ok(None) => {
                    tracing::trace!(
                        "data_sync_probe no_data_root_for_proof_peer offset={} peer={}",
                        chunk_offset,
                        peer_addr
                    );
                    request.excluded.insert(peer_addr);
                    self.ready_offsets.push_back(chunk_offset);
                    return DispatchStart::Skip;
                }
                Err(e) => {
                    warn!(
                        chunk.offset = %chunk_offset,
                        peer.address = %peer_addr,
                        error = %e,
                        "failed to resolve data_root for proof-signer fetch; excluding peer"
                    );
                    request.excluded.insert(peer_addr);
                    self.ready_offsets.push_back(chunk_offset);
                    return DispatchStart::Skip;
                }
            }
        };

        // Accounting + state transition only once we know we will dispatch.
        if let Ok(mut peers) = self.active_sync_peers.write()
            && let Some(pm) = peers.get_mut(&peer_addr)
        {
            pm.on_chunk_request_started();
        }

        let start_instant = Instant::now();
        let attempt_kind = if request.attempt_count == 0 {
            "fresh"
        } else {
            "retry"
        };
        if request.attempt_count == 0 {
            metrics::record_data_sync_unique_offset_attempted(self.ledger_id);
        }
        request.attempt_count = request.attempt_count.saturating_add(1);
        metrics::record_data_sync_attempt(self.ledger_id, attempt_kind);
        request.request_state = ChunkRequestState::Requested(peer_addr, start_instant);
        request.body_in_ingress = false;

        let chunk_fetcher = self.chunk_fetcher.clone();
        let tx = self.service_senders.data_sync.clone();
        let storage_module_id = self.storage_module.id;
        let ledger_id = self.ledger_id;
        let timeout = self.config.data_sync.chunk_request_timeout;
        let source_label: &'static str = if use_ledger_offset {
            "assigned"
        } else {
            "ingress_proof"
        };

        self.runtime_handle.spawn(
            async move {
                tracing::trace!(
                    "Fetching chunk {chunk_offset} from {api_addr} via {source_label} ({fetch_mode:?})"
                );

                let result = match fetch_mode {
                    FetchMode::LedgerOffset(ledger_chunk_offset) => {
                        chunk_fetcher
                            .fetch_chunk_by_ledger_offset(ledger_chunk_offset, api_addr, timeout)
                            .await
                    }
                    FetchMode::DataCommitment {
                        data_commitment,
                        tx_offset,
                    } => {
                        chunk_fetcher
                            .fetch_chunk_by_commitment(data_commitment, tx_offset, api_addr, timeout)
                            .await
                    }
                };

                let message = match result {
                    Ok(chunk) => {
                        metrics::record_data_sync_fetch_by_source(source_label, "success");
                        DataSyncServiceMessage::ChunkCompleted {
                            storage_module_id,
                            chunk_offset,
                            peer_address: peer_addr,
                            chunk,
                        }
                    }
                    Err(error) => {
                        let failure_kind = error.kind();
                        metrics::record_data_sync_fetch_by_source(
                            source_label,
                            failure_kind.as_metric_label(),
                        );
                        metrics::record_data_sync_fetch_failure(
                            ledger_id,
                            peer_addr,
                            failure_kind.as_metric_label(),
                        );
                        DataSyncServiceMessage::ChunkFailed {
                            storage_module_id,
                            chunk_offset,
                            peer_addr,
                            failure_kind,
                        }
                    }
                };

                // Service handles fetch credit + local write outcome (store / block / requeue).
                let _ = tx.send_traced(message);
            }
            .instrument(tracing::Span::current()),
        );
        DispatchStart::Started
    }

    /// Peer delivered the chunk body. Releases the peer slot and records the
    /// latency, but leaves the request in
    /// [`ChunkRequestState::Requested`] with `body_in_ingress` set until
    /// [`Self::mark_chunk_awaiting_durability`] / [`Self::mark_chunk_blocked`] /
    /// [`Self::requeue_after_local_write_failure`] /
    /// [`Self::on_ingressed_chunk_rejected`] decides the local outcome.
    /// `dispatch_next` sends only `Pending` offsets, so this one is not
    /// fetched again while ingress runs. A second call for the same delivery
    /// is an error and releases nothing. The returned record's `start_time`
    /// identifies this attempt in [`Self::body_awaiting_ingress`]. The success
    /// credit waits for [`Self::credit_accepted_delivery`].
    ///
    /// Peer delivery success must not be conflated with durable local storage.
    pub fn on_chunk_fetched(
        &mut self,
        chunk_offset: PartitionChunkOffset,
        peer_addr: IrysAddress,
    ) -> eyre::Result<ChunkTimeRecord> {
        let request = self.chunk_requests.get_mut(&chunk_offset).ok_or_else(|| {
            eyre::eyre!(
                "Chunk fetch completion for unknown offset: {:?}",
                chunk_offset
            )
        })?;

        let (expected_peer, start_instant) = match request.request_state {
            ChunkRequestState::Requested(addr, started) => (addr, started),
            ref invalid_state @ (ChunkRequestState::Pending
            | ChunkRequestState::AwaitingDurability(..)
            | ChunkRequestState::Completed
            | ChunkRequestState::Delayed { .. }
            | ChunkRequestState::Blocked(_)
            | ChunkRequestState::HeldByBodyWorker) => {
                return Err(eyre::eyre!(
                    "Invalid state for chunk fetch at offset {}: expected Requested, got {:?}",
                    chunk_offset,
                    invalid_state
                ));
            }
        };

        if expected_peer != peer_addr {
            return Err(eyre::eyre!("Peer mismatch for chunk {:?}", chunk_offset));
        }
        if request.body_in_ingress {
            return Err(eyre::eyre!(
                "Chunk {:?} from {} already released its peer slot",
                chunk_offset,
                peer_addr
            ));
        }
        request.body_in_ingress = true;

        let completion_time = Instant::now();
        let duration = completion_time.duration_since(start_instant);

        let completion_record = ChunkTimeRecord {
            chunk_offset,
            start_time: start_instant,
            completion_time,
            duration,
        };

        self.recent_chunk_times.push(completion_record.clone());

        // Free the slot now. The body is checked later, so the streak reset
        // and byte credit wait for `credit_accepted_delivery`.
        if let Ok(mut peers) = self.active_sync_peers.write()
            && let Some(peer_manager) = peers.get_mut(&peer_addr)
        {
            peer_manager.on_chunk_delivered(completion_record.clone());
        }

        Ok(completion_record)
    }

    /// The body `peer_addr` delivered passed validation and ingress. Clears
    /// that peer's failure streak and counts the bytes. The local write
    /// outcome does not matter here.
    pub fn credit_accepted_delivery(&self, peer_addr: IrysAddress) {
        if let Ok(mut peers) = self.active_sync_peers.write()
            && let Some(peer_manager) = peers.get_mut(&peer_addr)
        {
            peer_manager.on_delivery_accepted();
        }
    }

    /// True while the body `peer_addr` delivered for the attempt that started
    /// at `requested_at` is still in the spawned ingress. A `ChunkIngressed`
    /// result is applied only then, so a result from a replaced orchestrator
    /// or an earlier attempt cannot settle a newer one.
    pub fn body_awaiting_ingress(
        &self,
        chunk_offset: PartitionChunkOffset,
        peer_addr: IrysAddress,
        requested_at: Instant,
    ) -> bool {
        self.chunk_requests
            .get(&chunk_offset)
            .is_some_and(|request| {
                request.body_in_ingress
                    && matches!(
                        request.request_state,
                        ChunkRequestState::Requested(addr, started)
                            if addr == peer_addr && started == requested_at
                    )
            })
    }

    /// Local write entered the pending buffer. Durability is confirmed
    /// separately by the per-tick `reconcile_awaiting_durability` poll.
    pub fn mark_chunk_awaiting_durability(
        &mut self,
        chunk_offset: PartitionChunkOffset,
    ) -> eyre::Result<()> {
        let request = self.chunk_requests.get_mut(&chunk_offset).ok_or_else(|| {
            eyre::eyre!(
                "mark_chunk_awaiting_durability for unknown offset: {:?}",
                chunk_offset
            )
        })?;
        if !matches!(request.request_state, ChunkRequestState::Requested(..)) {
            return Err(eyre::eyre!(
                "mark_chunk_awaiting_durability expected Requested state at {:?}, got {:?}",
                chunk_offset,
                request.request_state
            ));
        }
        request.request_state = ChunkRequestState::AwaitingDurability(Instant::now());
        Ok(())
    }

    /// The body is in the sync staging buffer or the sweep queue.
    ///
    /// Accepts `Requested` and `Pending`. A peer disconnect can move a staged
    /// offset back to `Pending` before the contiguous run is published.
    pub fn note_body_buffered(&mut self, chunk_offset: PartitionChunkOffset) -> eyre::Result<()> {
        let request = self.chunk_requests.get_mut(&chunk_offset).ok_or_else(|| {
            eyre::eyre!("note_body_buffered for unknown offset: {:?}", chunk_offset)
        })?;
        if matches!(
            request.request_state,
            ChunkRequestState::AwaitingDurability(..) | ChunkRequestState::Completed
        ) {
            return Ok(());
        }
        if !matches!(
            request.request_state,
            ChunkRequestState::Requested(..) | ChunkRequestState::Pending
        ) {
            return Err(eyre::eyre!(
                "note_body_buffered expected an active fetch at {:?}, got {:?}",
                chunk_offset,
                request.request_state
            ));
        }
        request.request_state = ChunkRequestState::AwaitingDurability(Instant::now());
        Ok(())
    }

    /// A concurrent local writer already completed this offset before the
    /// fetched body reached the pending buffer.
    pub fn mark_chunk_already_durable(
        &mut self,
        chunk_offset: PartitionChunkOffset,
    ) -> eyre::Result<()> {
        let request = self.chunk_requests.get_mut(&chunk_offset).ok_or_else(|| {
            eyre::eyre!(
                "mark_chunk_already_durable for unknown offset: {:?}",
                chunk_offset
            )
        })?;
        if !matches!(request.request_state, ChunkRequestState::Requested(..)) {
            return Err(eyre::eyre!(
                "mark_chunk_already_durable expected Requested state at {:?}, got {:?}",
                chunk_offset,
                request.request_state
            ));
        }
        request.request_state = ChunkRequestState::Completed;
        Ok(())
    }

    /// Locally blocked; stop hot re-fetching this offset.
    pub fn mark_chunk_blocked(
        &mut self,
        chunk_offset: PartitionChunkOffset,
        reason: ChunkBlockReason,
    ) -> eyre::Result<()> {
        {
            let request = self.chunk_requests.get_mut(&chunk_offset).ok_or_else(|| {
                eyre::eyre!("mark_chunk_blocked for unknown offset: {:?}", chunk_offset)
            })?;
            if !matches!(request.request_state, ChunkRequestState::Requested(..)) {
                return Err(eyre::eyre!(
                    "mark_chunk_blocked expected Requested state at {:?}, got {:?}",
                    chunk_offset,
                    request.request_state
                ));
            }
            request.request_state = ChunkRequestState::Blocked(reason);
            // Clear peer exclusions — the failure is local, not peer-specific.
            request.excluded.clear();
        }
        self.trim_blocked_requests();
        Ok(())
    }

    /// Re-queue offsets blocked solely on [`ChunkBlockReason::MissingDataRootIndex`]
    /// for which `is_ready` returns true, up to `max` successes, lowest first.
    ///
    /// `max_probes` bounds how many times `is_ready` is called so a large still-
    /// unindexed Blocked backlog cannot force a full-map index walk every re-arm
    /// tick. Remaining Blocked offsets wait for a later pass (heal progress or
    /// lower offsets clearing).
    ///
    /// Returns the number of requests moved to [`ChunkRequestState::Pending`].
    pub fn unblock_missing_data_root_index_where(
        &mut self,
        max: usize,
        max_probes: usize,
        mut is_ready: impl FnMut(PartitionChunkOffset) -> bool,
    ) -> usize {
        if max == 0 || max_probes == 0 {
            return 0;
        }
        let mut offsets: Vec<PartitionChunkOffset> = self
            .chunk_requests
            .iter()
            .filter_map(|(&offset, request)| {
                matches!(
                    request.request_state,
                    ChunkRequestState::Blocked(ChunkBlockReason::MissingDataRootIndex)
                )
                .then_some(offset)
            })
            .collect();
        if offsets.is_empty() {
            return 0;
        }
        offsets.sort_unstable();
        let mut count = 0_usize;
        let now = Instant::now();
        for (probe_idx, offset) in offsets.into_iter().enumerate() {
            if count >= max || probe_idx >= max_probes {
                break;
            }
            if !is_ready(offset) {
                continue;
            }
            if let Some(request) = self.chunk_requests.get_mut(&offset) {
                request.request_state = ChunkRequestState::Pending;
                request.ready_since = now;
                request.excluded.clear();
                request.retry_round = 0;
                self.ready_offsets.push_back(offset);
                count += 1;
            }
        }
        count
    }

    /// Unconditionally re-queue up to `max` `MissingDataRootIndex` Blocked offsets
    /// (lowest first). Prefer [`Self::unblock_missing_data_root_index_where`] when
    /// a local readiness probe is available.
    pub fn unblock_missing_data_root_index(&mut self, max: usize) -> usize {
        // Unconditional path: every probe succeeds, so max_probes == max.
        self.unblock_missing_data_root_index_where(max, max, |_| true)
    }

    /// Local write failed for a non-blocking reason; re-queue without blaming the peer
    /// that just delivered the bytes.
    pub fn requeue_after_local_write_failure(
        &mut self,
        chunk_offset: PartitionChunkOffset,
    ) -> eyre::Result<()> {
        let request = self.chunk_requests.get_mut(&chunk_offset).ok_or_else(|| {
            eyre::eyre!(
                "requeue_after_local_write_failure for unknown offset: {:?}",
                chunk_offset
            )
        })?;
        if !matches!(request.request_state, ChunkRequestState::Requested(..)) {
            return Err(eyre::eyre!(
                "requeue_after_local_write_failure expected Requested at {:?}, got {:?}",
                chunk_offset,
                request.request_state
            ));
        }
        request.request_state = ChunkRequestState::Pending;
        request.ready_since = Instant::now();
        request.excluded.clear();
        request.retry_round = 0;
        self.ready_offsets.push_back(chunk_offset);
        // Do not add the delivering peer to `excluded` — they succeeded.
        Ok(())
    }

    #[tracing::instrument(level = "trace", skip_all, fields(
        chunk.offset = %chunk_offset,
        peer.address = %peer_addr
    ))]
    pub fn on_chunk_failed(
        &mut self,
        chunk_offset: PartitionChunkOffset,
        peer_addr: IrysAddress,
        failure_kind: crate::chunk_fetcher::ChunkFetchFailureKind,
    ) -> eyre::Result<()> {
        self.fail_requested(chunk_offset, peer_addr, failure_kind, false)
    }

    /// Ingress or local validation rejected a body that `peer_addr` already
    /// delivered. [`Self::on_chunk_fetched`] released that slot, so this
    /// records the failure on the peer without releasing a slot again. The
    /// offset is retried from another peer the same way as a failed fetch.
    #[tracing::instrument(level = "trace", skip_all, fields(
        chunk.offset = %chunk_offset,
        peer.address = %peer_addr
    ))]
    pub fn on_ingressed_chunk_rejected(
        &mut self,
        chunk_offset: PartitionChunkOffset,
        peer_addr: IrysAddress,
    ) -> eyre::Result<()> {
        self.fail_requested(
            chunk_offset,
            peer_addr,
            crate::chunk_fetcher::ChunkFetchFailureKind::InvalidResponse,
            true,
        )
    }

    /// `slot_released` is true when the body arrived and its slot was freed.
    fn fail_requested(
        &mut self,
        chunk_offset: PartitionChunkOffset,
        peer_addr: IrysAddress,
        failure_kind: crate::chunk_fetcher::ChunkFetchFailureKind,
        slot_released: bool,
    ) -> eyre::Result<()> {
        let all_peers_exhausted = {
            let request = self.chunk_requests.get_mut(&chunk_offset).ok_or_else(|| {
                eyre::eyre!("Chunk failure for unknown offset: {:?}", chunk_offset)
            })?;

            let expected_peer = match request.request_state {
                ChunkRequestState::Requested(addr, _) => addr,
                ChunkRequestState::Pending
                | ChunkRequestState::AwaitingDurability(..)
                | ChunkRequestState::Completed
                | ChunkRequestState::Delayed { .. }
                | ChunkRequestState::Blocked(_)
                | ChunkRequestState::HeldByBodyWorker => {
                    return Err(eyre::eyre!(
                        "Invalid request state for chunk failure: {:?}",
                        chunk_offset
                    ));
                }
            };

            if expected_peer != peer_addr {
                return Err(eyre::eyre!(
                    "Peer mismatch for chunk failure: {:?} expected peer: {} actual: {}",
                    chunk_offset,
                    expected_peer,
                    peer_addr
                ));
            }
            if request.body_in_ingress != slot_released {
                return Err(eyre::eyre!(
                    "Chunk failure at {:?} from {}: body_in_ingress is {}",
                    chunk_offset,
                    peer_addr,
                    request.body_in_ingress
                ));
            }

            request.excluded.insert(expected_peer);
            !self.current_peers.is_empty()
                && self
                    .current_peers
                    .iter()
                    .all(|peer| request.excluded.contains(peer))
        };

        if let Ok(mut peers) = self.active_sync_peers.write()
            && let Some(peer_manager) = peers.get_mut(&peer_addr)
        {
            match failure_kind {
                _ if slot_released => peer_manager.on_delivered_chunk_rejected(),
                crate::chunk_fetcher::ChunkFetchFailureKind::NotFound => {
                    peer_manager.on_chunk_not_found()
                }
                _ => peer_manager.on_chunk_request_failure(),
            }
        }

        let now = Instant::now();
        if all_peers_exhausted {
            self.delay_request(chunk_offset, now);
        } else if let Some(request) = self.chunk_requests.get_mut(&chunk_offset) {
            // A failed or currently unavailable offset remains unresolved, but
            // goes to the FIFO tail so other ready offsets and peers run first.
            request.request_state = ChunkRequestState::Pending;
            request.ready_since = now;
            self.ready_offsets.push_back(chunk_offset);
        }

        Ok(())
    }

    pub fn add_peer(&mut self, peer_addr: IrysAddress) {
        if !self.current_peers.contains(&peer_addr) {
            self.current_peers.push(peer_addr);
        }
    }

    /// Release every peer slot this orchestrator still holds. Call once,
    /// immediately before the orchestrator is removed. Rows that already left
    /// `Requested`, and rows whose body is in ingress, are skipped: their
    /// completion released the slot. A missing manager is skipped. The
    /// request map is left unchanged because the orchestrator is dropped next.
    /// A second call while those rows stay `Requested` would release the
    /// same slots again.
    pub(super) fn release_in_flight_peer_slots(&mut self) {
        let peers: Vec<IrysAddress> = self
            .chunk_requests
            .values()
            .filter_map(|request| match request.request_state {
                ChunkRequestState::Requested(_, _) if request.body_in_ingress => None,
                ChunkRequestState::Requested(addr, _) => Some(addr),
                ChunkRequestState::Pending
                | ChunkRequestState::AwaitingDurability(..)
                | ChunkRequestState::Completed
                | ChunkRequestState::Delayed { .. }
                | ChunkRequestState::Blocked(_)
                | ChunkRequestState::HeldByBodyWorker => None,
            })
            .collect();
        let Ok(mut managers) = self.active_sync_peers.write() else {
            return;
        };
        for addr in peers {
            if let Some(manager) = managers.get_mut(&addr) {
                manager.release_abandoned_request();
            }
        }
    }

    pub fn remove_peer(&mut self, peer_addr: IrysAddress) {
        self.current_peers.retain(|&addr| addr != peer_addr);

        let now = Instant::now();
        let mut ready = Vec::new();
        for (offset, request) in &mut self.chunk_requests {
            // A delivered body is already off the network. Its ingress result
            // still decides the offset; requeuing it here would fetch it twice.
            if let ChunkRequestState::Requested(addr, _) = request.request_state
                && addr == peer_addr
                && !request.body_in_ingress
            {
                request.request_state = ChunkRequestState::Pending;
                request.ready_since = now;
                request.excluded.insert(addr);
                ready.push(*offset);
            }
        }
        self.sort_in_scan_order(&mut ready);
        self.ready_offsets.extend(ready);
    }

    pub fn get_metrics(&self) -> OrchestrationMetrics {
        let (ready, active, awaiting_durability, completed, delayed, blocked) = self
            .chunk_requests
            .values()
            .fold(
                (0, 0, 0, 0, 0, 0),
                |(r, a, w, c, d, b), request| match request.request_state {
                    ChunkRequestState::Pending => (r + 1, a, w, c, d, b),
                    ChunkRequestState::Requested(_, _) => (r, a + 1, w, c, d, b),
                    ChunkRequestState::AwaitingDurability(..) => (r, a, w + 1, c, d, b),
                    ChunkRequestState::Completed => (r, a, w, c + 1, d, b),
                    ChunkRequestState::Delayed { .. } => (r, a, w, c, d + 1, b),
                    ChunkRequestState::Blocked(_) => (r, a, w, c, d, b + 1),
                    // Held work is left out of the ready and blocked counters.
                    // Discovery counts it toward `max_pending_chunk_requests`.
                    ChunkRequestState::HeldByBodyWorker => (r, a, w, c, d, b),
                },
            );

        let now = Instant::now();
        let oldest_ready_age = self
            .chunk_requests
            .values()
            .filter_map(|request| match request.request_state {
                ChunkRequestState::Pending => Some(now.duration_since(request.ready_since)),
                _ => None,
            })
            .max()
            .unwrap_or_default();

        let total_throughput_bps = if let Ok(peers) = self.active_sync_peers.read() {
            self.current_peers
                .iter()
                .filter_map(|addr| peers.get(addr))
                .map(PeerBandwidthManager::current_bandwidth_bps)
                .sum()
        } else {
            0
        };

        OrchestrationMetrics {
            total_peers: self.current_peers.len(),
            pending_requests: ready,
            active_requests: active,
            awaiting_durability_requests: awaiting_durability,
            completed_requests: completed,
            delayed_requests: delayed,
            blocked_requests: blocked,
            oldest_ready_age,
            scan_cursor: self.scan_cursor,
            total_throughput_bps,
        }
    }
}

#[derive(Debug)]
pub struct OrchestrationMetrics {
    pub total_peers: usize,
    pub pending_requests: usize,
    pub active_requests: usize,
    pub awaiting_durability_requests: usize,
    pub completed_requests: usize,
    pub delayed_requests: usize,
    pub blocked_requests: usize,
    pub oldest_ready_age: Duration,
    pub scan_cursor: u32,
    pub total_throughput_bps: u64,
}

/// One pass over the live interval map inside `[range_start, range_end]`.
struct EntropyScan {
    /// Entropy sub-ranges to enqueue. Width is at most `max_offsets` chunks.
    runs: Vec<(u32, u32)>,
    /// Inclusive edge of the intervals this pass walked.
    /// Forward: highest offset. Reverse: lowest offset.
    visited_edge: Option<u32>,
    /// The interval count hit `max_offsets` before the chunk budget did.
    interval_budget_spent: bool,
}

/// Entropy runs from the live interval map inside `[range_start, range_end]`.
///
/// Each overlapping interval costs one visit, whether or not it is Entropy.
/// Entropy width is still at most `max_offsets` chunks. A packing tail is
/// clipped to that chunk budget, and that clip does not set
/// [`EntropyScan::interval_budget_spent`]: the caller resumes inside the run.
/// Reverse scans keep the high end of each run. Pending writes are not in
/// this map; the caller checks chunk type before it enqueues.
fn entropy_scan_runs(
    storage_module: &StorageModule,
    range_start: u32,
    range_end: u32,
    reverse: bool,
    max_offsets: usize,
) -> EntropyScan {
    if max_offsets == 0 || range_start > range_end {
        return EntropyScan {
            runs: Vec::new(),
            visited_edge: None,
            interval_budget_spent: false,
        };
    }
    let intervals = storage_module.label_runs();
    let query = ii(
        PartitionChunkOffset::from(range_start),
        PartitionChunkOffset::from(range_end),
    );
    let mut left = max_offsets;
    let mut intervals_seen = 0_usize;
    let mut runs = Vec::new();
    let mut visited_edge = None;
    let mut interval_budget_spent = false;
    let mut push = |start: u32, end: u32, chunk_type: &ChunkType| -> bool {
        let clipped_start = start.max(range_start);
        let clipped_end = end.min(range_end);
        if clipped_start > clipped_end {
            return false;
        }
        intervals_seen += 1;
        visited_edge = Some(if reverse { clipped_start } else { clipped_end });
        if *chunk_type == ChunkType::Entropy
            && let Some(run) =
                clip_entropy_run(start, end, range_start, range_end, reverse, &mut left)
        {
            runs.push(run);
        }
        // The chunk budget stopped inside an entropy interval. Leave the
        // interval flag clear so the cursor stays on the returned prefix.
        if left == 0 {
            return true;
        }
        if intervals_seen >= max_offsets {
            interval_budget_spent = true;
            return true;
        }
        false
    };
    if reverse {
        for (interval, chunk_type) in intervals.overlapping(query).rev() {
            if push(*interval.start(), *interval.end(), chunk_type) {
                break;
            }
        }
    } else {
        for (interval, chunk_type) in intervals.overlapping(query) {
            if push(*interval.start(), *interval.end(), chunk_type) {
                break;
            }
        }
    }
    EntropyScan {
        runs,
        visited_edge,
        interval_budget_spent,
    }
}

/// Next cursor after an interval window. Forward steps past `edge`. Reverse
/// steps below it. Either side wraps to the start of that direction.
fn resume_scan_cursor(edge: u32, max_offset: u32, reverse: bool) -> u32 {
    if reverse {
        if edge == 0 { u32::MAX } else { edge - 1 }
    } else if edge >= max_offset {
        0
    } else {
        edge + 1
    }
}

fn clip_entropy_run(
    start: u32,
    end: u32,
    range_start: u32,
    range_end: u32,
    reverse: bool,
    left: &mut usize,
) -> Option<(u32, u32)> {
    if *left == 0 {
        return None;
    }
    let start = start.max(range_start);
    let end = end.min(range_end);
    if start > end {
        return None;
    }
    let span = u64::from(end - start) + 1;
    let take = span.min(u64::from(u32::MAX)).min(*left as u64) as u32;
    if take == 0 {
        return None;
    }
    let delta = take - 1;
    let run = if reverse {
        (end - delta, end)
    } else {
        (start, start + delta)
    };
    *left -= take as usize;
    Some(run)
}

/// Mix of live Entropy spans. Zero only when the map has none. Count is mixed
/// in so a split that preserves a start/end xor still changes the value, and
/// the low bit is forced on so a non-empty map cannot hash to zero.
fn entropy_interval_fingerprint(storage_module: &StorageModule) -> u64 {
    #[cfg(test)]
    ENTROPY_FINGERPRINT_CALLS.with(|calls| calls.set(calls.get().saturating_add(1)));
    let intervals = storage_module.label_runs();
    let mut mixed = 0_u64;
    let mut count = 0_u64;
    for (interval, chunk_type) in intervals.iter() {
        if *chunk_type != ChunkType::Entropy {
            continue;
        }
        count += 1;
        let start = u64::from(*interval.start());
        let end = u64::from(*interval.end());
        mixed = mixed
            .wrapping_mul(0x9E37_79B1_85EB_CA87)
            .wrapping_add(start)
            .wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
            .wrapping_add(end);
    }
    if count == 0 {
        return 0;
    }
    mixed
        .wrapping_mul(0x9E37_79B1_85EB_CA87)
        .wrapping_add(count)
        | 1
}

#[cfg(test)]
thread_local! {
    static ENTROPY_FINGERPRINT_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Zero the entropy-walk counter. Tests that share a worker thread start here.
#[cfg(test)]
pub(crate) fn reset_entropy_fingerprint_calls() {
    ENTROPY_FINGERPRINT_CALLS.with(|calls| calls.set(0));
}

/// Walks of the live interval map since the last reset on this thread.
#[cfg(test)]
pub(crate) fn entropy_fingerprint_calls() -> u64 {
    ENTROPY_FINGERPRINT_CALLS.with(std::cell::Cell::get)
}

#[cfg(test)]
mod tests;
