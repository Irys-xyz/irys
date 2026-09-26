use crate::{
    DataSyncServiceMessage, chunk_fetcher::ChunkFetcher,
    data_sync_service::peer_bandwidth_manager::PeerBandwidthManager, metrics,
    services::ServiceSenders,
};
use irys_domain::{BlockTreeReadGuard, ChunkTimeRecord, ChunkType, CircularBuffer, StorageModule};
use irys_types::{
    DataLedger, IrysAddress, LedgerChunkOffset, NodeConfig, PartitionChunkOffset, SendTraced as _,
    hardfork_config::DataLedgerLookup,
};
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

/// Retry an offset only after other ready work has had an opportunity to run.
/// While its in-memory request record is retained, the delay is monotonic,
/// doubles after each exhausted peer cycle, and is capped so data that appears
/// later is eventually rediscovered.
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

/// How to request a missing body from a selected peer.
///
/// Assignees use ledger-offset; ingress-proof signers (often no SM) use data_root.
#[derive(Debug, Clone, Copy)]
enum FetchMode {
    LedgerOffset(LedgerChunkOffset),
    DataRoot {
        data_root: irys_types::DataRoot,
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
}

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
    ) -> Self {
        let ledger_id = storage_module
            .partition_assignment()
            .expect("storage_module should have a partition assignment")
            .ledger_id
            .expect("storage_module should be assigned to a data ledger");

        Self {
            storage_module,
            chunk_requests: Default::default(),
            ready_offsets: Default::default(),
            scan_cursor: if Self::ledger_prioritizes_write_frontier(ledger_id) {
                u32::MAX
            } else {
                0
            },
            recent_chunk_times: CircularBuffer::new(8000),
            current_peers: Default::default(),
            block_tree,
            active_sync_peers: sync_peers,
            service_senders: service_senders.clone(),
            ledger_id,
            chunk_fetcher, // Store the chunk fetcher
            config,
            runtime_handle,
        }
    }

    pub(crate) const fn ledger_id(&self) -> u32 {
        self.ledger_id
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
            self.populate_request_queue();
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
        self.ready_offsets.extend(ready);
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
        // Peer exclusions are cleared here, at the beginning of a new attempt
        // cycle, never in the same transition that handled a 404.
        let due: Vec<_> = self
            .chunk_requests
            .iter()
            .filter_map(|(&offset, request)| match request.request_state {
                ChunkRequestState::Delayed { retry_at, .. } if retry_at <= now => Some(offset),
                _ => None,
            })
            .collect();
        for offset in due {
            if let Some(request) = self.chunk_requests.get_mut(&offset) {
                request.excluded.clear();
                request.request_state = ChunkRequestState::Pending;
                request.ready_since = now;
                self.ready_offsets.push_back(offset);
            }
        }

        // Ready and in-flight work share the configured hot-work budget.
        // Delayed entries do not consume it, otherwise a sparse prefix could
        // prevent discovery of later Entropy forever.
        let hot_count = self
            .chunk_requests
            .values()
            .filter(|request| {
                matches!(
                    request.request_state,
                    ChunkRequestState::Pending
                        | ChunkRequestState::Requested(..)
                        | ChunkRequestState::AwaitingDurability(..)
                )
            })
            .count();

        let max_chunk_offset = self.get_max_chunk_offset();
        let pa = self.storage_module.partition_assignment().unwrap();

        let Some((max_chunk_offset, _)) = max_chunk_offset else {
            // Not requests needed
            tracing::trace!(
                "No chunk requests needed for ledger:{:?} slot_index:{:?}",
                pa.ledger_id,
                pa.slot_index
            );
            return;
        };

        let max_requests = self.config.data_sync.max_pending_chunk_requests as usize;
        let mut requests_to_add = max_requests.saturating_sub(hot_count);

        // Local Entropy intervals do not distinguish a natural term-ledger gap
        // from a required body that is temporarily unavailable. Keep both
        // unresolved: fair circular discovery plus delayed retries provides
        // eventual reconciliation without falsely marking a 404 synchronized.
        let entropy_intervals = self.storage_module.get_intervals(ChunkType::Entropy);

        let max_offset = *max_chunk_offset;

        // Term ledgers (especially Submit) are a small write head on a packed
        // entropy tail. A low-to-high Entropy walk spends the hot budget on
        // the prefix, so the newest migrated offset is not even queued — the
        // replica looks one chunk short of the frontier. Enqueue that write
        // head first. The service dispatcher also visits term SMs before
        // Publish so a permanent backlog cannot skip this offset; Publish
        // later copies Submit data and needs Submit replicas on the frontier.
        if requests_to_add > 0
            && self.prioritizes_write_frontier()
            && entropy_intervals
                .iter()
                .any(|interval| *interval.start() <= max_offset && max_offset <= *interval.end())
            && self.enqueue_pending(PartitionChunkOffset::from(max_offset), now, true)
        {
            requests_to_add -= 1;
        }

        if requests_to_add == 0 {
            return;
        }

        let max_probes = max_requests.saturating_mul(4).max(max_requests);
        let mut probes = 0_usize;
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
                for interval in entropy_intervals.iter().rev() {
                    let start = (*interval.start()).max(range_start);
                    let end = (*interval.end()).min(range_end).min(max_offset);
                    if start > end {
                        continue;
                    }
                    for interval_step in (start..=end).rev() {
                        if self.note_scan_offset(
                            interval_step,
                            max_offset,
                            true,
                            now,
                            &mut probes,
                            max_probes,
                            &mut requests_to_add,
                        ) {
                            break 'scan;
                        }
                    }
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
                for interval in &entropy_intervals {
                    let start = (*interval.start()).max(range_start);
                    let end = (*interval.end()).min(range_end).min(max_offset);
                    if start > end {
                        continue;
                    }
                    for interval_step in start..=end {
                        if self.note_scan_offset(
                            interval_step,
                            max_offset,
                            false,
                            now,
                            &mut probes,
                            max_probes,
                            &mut requests_to_add,
                        ) {
                            break 'scan;
                        }
                    }
                }
            }
        }
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
        if self.enqueue_pending(PartitionChunkOffset::from(interval_step), now, false) {
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

        while let Some(chunk_offset) = self.ready_offsets.pop_front() {
            let Some((excluded, all_peers_exhausted)) = self
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
                        && self
                            .current_peers
                            .iter()
                            .all(|peer| chunk_request.excluded.contains(peer));
                    Some((chunk_request.excluded.clone(), exhausted))
                })
            else {
                continue;
            };

            if all_peers_exhausted {
                self.delay_request(chunk_offset, Instant::now());
                continue;
            }

            let Some(peer_address) = self.find_best_peer(Some(&excluded)) else {
                // Eligible peers exist but currently have no concurrency. Keep
                // FIFO position for the next service tick without spinning.
                self.ready_offsets.push_back(chunk_offset);
                return false;
            };

            if self.dispatch_chunk_request(chunk_offset, peer_address) {
                return true;
            }
        }

        false
    }

    fn retry_delay(chunk_offset: PartitionChunkOffset, retry_round: u32) -> Duration {
        let exponent = retry_round.saturating_sub(1).min(6);
        let base = RETRY_BACKOFF_INITIAL
            .saturating_mul(1_u32 << exponent)
            .min(RETRY_BACKOFF_MAX);
        // Stable per-offset jitter avoids synchronized retry waves without a
        // random generator or wall-clock dependency. Keep the capped maximum.
        let jitter_window_ms = (base.as_millis() / 4) as u64;
        let jitter_ms = if jitter_window_ms == 0 {
            0
        } else {
            u64::from(*chunk_offset).wrapping_mul(0x9e37_79b9) % (jitter_window_ms + 1)
        };
        base.saturating_add(Duration::from_millis(jitter_ms))
            .min(RETRY_BACKOFF_MAX)
    }

    fn delay_request(&mut self, chunk_offset: PartitionChunkOffset, now: Instant) {
        let Some(request) = self.chunk_requests.get_mut(&chunk_offset) else {
            return;
        };
        request.retry_round = request.retry_round.saturating_add(1);
        let retry_round = request.retry_round;
        let delay = Self::retry_delay(chunk_offset, retry_round);
        request.request_state = ChunkRequestState::Delayed {
            retry_at: now + delay,
        };
        metrics::record_data_sync_retry_delay(self.ledger_id, delay);
        metrics::record_data_sync_peers_exhausted(self.ledger_id);
        self.trim_delayed_requests();
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
        let ledger_range = self
            .storage_module
            .get_storage_module_ledger_offsets()
            .expect("storage module should be assigned to a ledger");

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
    ) -> bool {
        let Some(request) = self.chunk_requests.get_mut(&chunk_offset) else {
            return false;
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
                    return false;
                }
            };
            let Some(peer_manager) = peers.get(&peer_addr) else {
                // The peer can disappear between selection and dispatch. Keep
                // the still-Pending offset in the FIFO for another peer.
                self.ready_offsets.push_back(chunk_offset);
                return false;
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
            let start_ledger_offset = u64::from(
                self.storage_module
                    .get_storage_module_ledger_offsets()
                    .unwrap()
                    .start(),
            );
            let ledger_chunk_offset =
                LedgerChunkOffset::from(start_ledger_offset + u64::from(chunk_offset));
            FetchMode::LedgerOffset(ledger_chunk_offset)
        } else {
            match self.storage_module.data_root_and_tx_offset_at(chunk_offset) {
                Ok(Some((data_root, tx_offset))) => FetchMode::DataRoot {
                    data_root,
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
                    return false;
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
                    return false;
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
                    FetchMode::DataRoot {
                        data_root,
                        tx_offset,
                    } => {
                        chunk_fetcher
                            .fetch_chunk_by_data_root(data_root, tx_offset, api_addr, timeout)
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
        true
    }

    /// Peer delivered the chunk body. Credits peer bandwidth stats but leaves
    /// the request in [`ChunkRequestState::Requested`] until
    /// [`Self::mark_chunk_awaiting_durability`] / [`Self::mark_chunk_blocked`] /
    /// [`Self::requeue_after_local_write_failure`] decides the local outcome.
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
            | ChunkRequestState::Blocked(_)) => {
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

        let completion_time = Instant::now();
        let duration = completion_time.duration_since(start_instant);

        let completion_record = ChunkTimeRecord {
            chunk_offset,
            start_time: start_instant,
            completion_time,
            duration,
        };

        self.recent_chunk_times.push(completion_record.clone());

        // Credit the peer for successful delivery regardless of local write outcome.
        if let Ok(mut peers) = self.active_sync_peers.write()
            && let Some(peer_manager) = peers.get_mut(&peer_addr)
        {
            peer_manager.on_chunk_request_completed(completion_record.clone());
        }

        Ok(completion_record)
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
                | ChunkRequestState::Blocked(_) => {
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

    pub fn remove_peer(&mut self, peer_addr: IrysAddress) {
        self.current_peers.retain(|&addr| addr != peer_addr);

        let now = Instant::now();
        let mut ready = Vec::new();
        for (offset, request) in &mut self.chunk_requests {
            if let ChunkRequestState::Requested(addr, _) = request.request_state
                && addr == peer_addr
            {
                request.request_state = ChunkRequestState::Pending;
                request.ready_since = now;
                request.excluded.insert(addr);
                ready.push(*offset);
            }
        }
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

#[cfg(test)]
mod tests;
