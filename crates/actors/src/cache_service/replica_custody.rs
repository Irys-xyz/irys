//! Replica custody for cached chunk bodies.
//!
//! A cached body that this node neither stores, proves nor owes may be the
//! only copy in the network. The capacity prune releases such a body only
//! once, for every live ledger slot it occupies, an assignee of that slot
//! other than this node attests a crash-safe placement of exactly that
//! chunk. Until then the body is held and re-announced.

use super::TxLedgerSpan;
use crate::chunk_fetcher::{
    ChunkFetcher, HttpChunkFetcher, MAX_PERSISTED_QUERY_ENTRIES, PersistedQueryEntry,
};
use futures::StreamExt as _;
use irys_database::data_ledger::LedgerSlot;
use irys_domain::{BlockTreeReadGuard, EpochSnapshot, PeerList};
use irys_types::{DataLedger, DataRoot, IrysAddress, TxChunkOffset};
use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{debug, warn};

/// One chunk position: a partition-relative offset in a ledger slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SlotPoint {
    pub ledger: DataLedger,
    pub slot_index: usize,
    pub offset: u32,
}

/// One cached body the pass would delete, with every slot position its
/// root's ledger spans give it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageCandidate {
    pub data_root: DataRoot,
    pub tx_offset: TxChunkOffset,
    pub points: Vec<SlotPoint>,
}

/// Slot positions of `tx_offset` across `spans`, ascending and unique. A
/// span too short to reach the offset, or a position that does not fit,
/// gives none.
pub(super) fn slot_points(
    spans: &[TxLedgerSpan],
    tx_offset: TxChunkOffset,
    chunks_per_partition: u64,
) -> Vec<SlotPoint> {
    let raw = u64::from(*tx_offset);
    let mut points: Vec<SlotPoint> = spans
        .iter()
        .filter(|span| raw < span.chunk_count)
        .filter_map(|span| {
            let ledger_offset = (*span.start).checked_add(raw)?;
            let slot_index =
                usize::try_from(ledger_offset.checked_div(chunks_per_partition)?).ok()?;
            let offset = u32::try_from(ledger_offset.checked_rem(chunks_per_partition)?).ok()?;
            Some(SlotPoint {
                ledger: span.ledger,
                slot_index,
                offset,
            })
        })
        .collect();
    points.sort_unstable();
    points.dedup();
    points
}

/// Whether some slot that one of `spans` covers is `live`. A span whose
/// slots cannot be computed counts as live: unknown keeps.
pub(super) fn spans_touch_live_slot(
    spans: &[TxLedgerSpan],
    chunks_per_partition: u64,
    live: &dyn Fn(DataLedger, usize) -> bool,
) -> bool {
    spans.iter().any(|span| {
        let Some(last_chunk) = span.chunk_count.checked_sub(1) else {
            return false;
        };
        let first = (*span.start).checked_div(chunks_per_partition);
        let last = (*span.start)
            .checked_add(last_chunk)
            .and_then(|end| end.checked_div(chunks_per_partition));
        let (Some(first), Some(last)) = (first, last) else {
            return true;
        };
        (first..=last).any(|slot| {
            usize::try_from(slot)
                .ok()
                .is_none_or(|slot| live(span.ledger, slot))
        })
    })
}

/// Positions that some assignee other than `local` attests. This node never
/// witnesses its own custody, even if it is also an assignee.
pub fn remote_witnesses(
    attestations: &[(IrysAddress, SlotPoint)],
    local: IrysAddress,
) -> BTreeSet<SlotPoint> {
    attestations
        .iter()
        .filter(|(assignee, _)| *assignee != local)
        .map(|(_, point)| *point)
        .collect()
}

/// Whether a body may be released: it has at least one position, and every
/// position in a live slot is `witnessed`. Positions in expired slots need
/// no witness. A body with no position at all is kept, because a missing
/// span means its placement is unknown, not absent.
pub fn chunk_released(
    points: &[SlotPoint],
    live: impl Fn(&SlotPoint) -> bool,
    witnessed: &BTreeSet<SlotPoint>,
) -> bool {
    !points.is_empty()
        && points
            .iter()
            .filter(|point| live(point))
            .all(|point| witnessed.contains(point))
}

/// Answers which cached bodies a prune pass may delete. Synchronous: the
/// pass runs on its own `std::thread`.
pub trait ReplicaCoverage: Send + Sync + std::fmt::Debug {
    /// The key of every candidate that is released.
    fn released(&self, candidates: &[CoverageCandidate]) -> BTreeSet<(DataRoot, TxChunkOffset)>;
}

/// A ledger slot as this node sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotCustodians {
    pub expired: bool,
    /// Mining addresses of the slot's assignees, ascending and unique.
    pub assignees: Vec<IrysAddress>,
}

/// Where coverage learns slot assignments and peer addresses.
pub trait CustodyTopology: Send + Sync + std::fmt::Debug {
    /// `None` when the canonical snapshot has no such ledger or slot.
    fn slot(&self, ledger: DataLedger, slot_index: usize) -> Option<SlotCustodians>;
    /// API address of the peer that mines as `miner`, if this node knows it.
    fn api_addr(&self, miner: &IrysAddress) -> Option<SocketAddr>;
}

/// The slot of `ledger` at `slot_index`, when the ledger is active and the
/// slot exists. `Ledgers::get_slots` panics for an inactive term ledger.
pub(super) fn ledger_slot(
    snapshot: &EpochSnapshot,
    ledger: DataLedger,
    slot_index: usize,
) -> Option<&LedgerSlot> {
    if !snapshot.ledgers.active_ledgers().contains(&ledger) {
        return None;
    }
    snapshot.ledgers.get_slots(ledger).get(slot_index)
}

/// Production topology: the canonical epoch snapshot and the peer list.
#[derive(Debug, Clone)]
pub struct ChainCustodyTopology {
    block_tree_guard: BlockTreeReadGuard,
    peer_list: PeerList,
}

impl ChainCustodyTopology {
    pub fn new(block_tree_guard: BlockTreeReadGuard, peer_list: PeerList) -> Self {
        Self {
            block_tree_guard,
            peer_list,
        }
    }
}

impl CustodyTopology for ChainCustodyTopology {
    fn slot(&self, ledger: DataLedger, slot_index: usize) -> Option<SlotCustodians> {
        let snapshot = self.block_tree_guard.read().canonical_epoch_snapshot();
        let slot = ledger_slot(&snapshot, ledger, slot_index)?;
        let assignees: BTreeSet<IrysAddress> = slot
            .partitions
            .iter()
            .filter_map(|hash| snapshot.partition_assignments.get_assignment(*hash))
            .map(|assignment| assignment.miner_address)
            .collect();
        Some(SlotCustodians {
            expired: slot.is_expired,
            assignees: assignees.into_iter().collect(),
        })
    }

    fn api_addr(&self, miner: &IrysAddress) -> Option<SocketAddr> {
        self.peer_list
            .peer_by_mining_address(miner)
            .map(|peer| peer.address.api)
    }
}

/// Bounds on one pass's attestation round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoverageLimits {
    /// Per request: a slower assignee attests nothing this pass.
    pub request_timeout: Duration,
    /// Whole round: requests still pending attest nothing this pass.
    pub pass_deadline: Duration,
    pub max_in_flight: usize,
    pub max_entries_per_request: usize,
}

impl Default for CoverageLimits {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(5),
            pass_deadline: Duration::from_secs(30),
            max_in_flight: 8,
            max_entries_per_request: MAX_PERSISTED_QUERY_ENTRIES,
        }
    }
}

/// One batched question to one assignee about one slot.
struct SlotQuery {
    ledger: DataLedger,
    slot_index: usize,
    assignee: IrysAddress,
    api_addr: SocketAddr,
    entries: Vec<PersistedQueryEntry>,
}

/// Gathers attestations from remote assignees over HTTP, once per pass.
#[derive(Debug)]
pub struct HttpReplicaCoverage {
    topology: Arc<dyn CustodyTopology>,
    local_miner: IrysAddress,
    fetchers: BTreeMap<DataLedger, Arc<dyn ChunkFetcher>>,
    limits: CoverageLimits,
    runtime: tokio::runtime::Handle,
    next_query: AtomicUsize,
}

impl HttpReplicaCoverage {
    pub fn new(
        topology: Arc<dyn CustodyTopology>,
        local_miner: IrysAddress,
        fetchers: BTreeMap<DataLedger, Arc<dyn ChunkFetcher>>,
        limits: CoverageLimits,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        Self {
            topology,
            local_miner,
            fetchers,
            limits,
            runtime,
            next_query: AtomicUsize::new(0),
        }
    }

    /// One `HttpChunkFetcher` per data ledger.
    pub fn http_fetchers(
        max_peer_response_bytes: u64,
    ) -> BTreeMap<DataLedger, Arc<dyn ChunkFetcher>> {
        DataLedger::ALL
            .into_iter()
            .map(|ledger| {
                let fetcher: Arc<dyn ChunkFetcher> = Arc::new(HttpChunkFetcher::new(
                    ledger.get_id(),
                    max_peer_response_bytes,
                ));
                (ledger, fetcher)
            })
            .collect()
    }

    fn slots_of(
        &self,
        candidates: &[CoverageCandidate],
    ) -> BTreeMap<(DataLedger, usize), Option<SlotCustodians>> {
        let mut slots = BTreeMap::new();
        for point in candidates.iter().flat_map(|candidate| &candidate.points) {
            slots
                .entry((point.ledger, point.slot_index))
                .or_insert_with(|| self.topology.slot(point.ledger, point.slot_index));
        }
        slots
    }

    /// One query per remote assignee, live slot and `max_entries_per_request`
    /// offsets. A position two candidates claim for different chunks is
    /// left out, so neither is released on it.
    fn queries(
        &self,
        candidates: &[CoverageCandidate],
        slots: &BTreeMap<(DataLedger, usize), Option<SlotCustodians>>,
        live: impl Fn(&SlotPoint) -> bool,
    ) -> Vec<SlotQuery> {
        let mut asked: BTreeMap<
            (DataLedger, usize),
            BTreeMap<u32, Option<(DataRoot, TxChunkOffset)>>,
        > = BTreeMap::new();
        for candidate in candidates {
            let chunk = Some((candidate.data_root, candidate.tx_offset));
            for point in candidate.points.iter().filter(|point| live(point)) {
                asked
                    .entry((point.ledger, point.slot_index))
                    .or_default()
                    .entry(point.offset)
                    .and_modify(|claimed| {
                        if *claimed != chunk {
                            *claimed = None;
                        }
                    })
                    .or_insert(chunk);
            }
        }
        let mut queries = Vec::new();
        for ((ledger, slot_index), offsets) in asked {
            let Some(Some(slot)) = slots.get(&(ledger, slot_index)) else {
                continue;
            };
            let entries: Vec<PersistedQueryEntry> = offsets
                .into_iter()
                .filter_map(|(offset, claimed)| {
                    let (data_root, tx_offset) = claimed?;
                    Some(PersistedQueryEntry {
                        offset,
                        data_root,
                        tx_offset,
                    })
                })
                .collect();
            let assignees: Vec<_> = slot
                .assignees
                .iter()
                .filter(|assignee| **assignee != self.local_miner)
                .filter_map(|assignee| {
                    let Some(api_addr) = self.topology.api_addr(assignee) else {
                        debug!(
                            ?assignee,
                            "Assignee has no known API address; no attestation"
                        );
                        return None;
                    };
                    Some((*assignee, api_addr))
                })
                .collect();
            for batch in entries.chunks(self.limits.max_entries_per_request.max(1)) {
                for &(assignee, api_addr) in &assignees {
                    queries.push(SlotQuery {
                        ledger,
                        slot_index,
                        assignee,
                        api_addr,
                        // clone: each request owns its batch while in flight.
                        entries: batch.to_vec(),
                    });
                }
            }
        }
        queries
    }

    /// Runs `queries` at most `max_in_flight` at a time, each cut at
    /// `request_timeout` and all at `pass_deadline`. A failed, late or
    /// unanswered query attests nothing.
    fn gather(&self, queries: Vec<SlotQuery>) -> Vec<(IrysAddress, SlotPoint)> {
        self.runtime.block_on(self.gather_async(queries))
    }

    async fn gather_async(&self, mut queries: Vec<SlotQuery>) -> Vec<(IrysAddress, SlotPoint)> {
        if queries.is_empty() {
            return Vec::new();
        }
        let query_count = queries.len();
        let start = self.next_query.load(Ordering::Relaxed) % query_count;
        queries.rotate_left(start);
        let mut attempted = 0;
        let limits = self.limits;
        let requests = queries.into_iter().filter_map(|query| {
            attempted += 1;
            let fetcher = self.fetchers.get(&query.ledger)?;
            Some(async move {
                let answer = tokio::time::timeout(
                    limits.request_timeout,
                    fetcher.fetch_persisted_offsets(
                        query.slot_index,
                        &query.entries,
                        query.api_addr,
                        limits.request_timeout,
                    ),
                )
                .await;
                (query, answer)
            })
        });
        let round = async move {
            let mut answers = Vec::new();
            let mut in_flight =
                futures::stream::iter(requests).buffer_unordered(limits.max_in_flight.max(1));
            let deadline = tokio::time::sleep(limits.pass_deadline);
            tokio::pin!(deadline);
            loop {
                tokio::select! {
                    next = in_flight.next() => match next {
                        Some(answer) => answers.push(answer),
                        None => break,
                    },
                    () = &mut deadline => {
                        debug!("Replica coverage deadline reached; unanswered queries attest nothing");
                        break;
                    }
                }
            }
            answers
        };
        let answers = round.await;
        // Passes are serialized; only the cursor survives a truncated round.
        self.next_query
            .store((start + attempted) % query_count, Ordering::Relaxed);
        let mut attestations = Vec::new();
        for (query, answer) in answers {
            let offsets = match answer {
                Ok(Ok(offsets)) => offsets,
                Ok(Err(error)) => {
                    debug!(assignee = ?query.assignee, ?error, "No attestation from assignee");
                    continue;
                }
                Err(_elapsed) => {
                    debug!(assignee = ?query.assignee, "Attestation request timed out");
                    continue;
                }
            };
            let asked: BTreeSet<u32> = query.entries.iter().map(|entry| entry.offset).collect();
            attestations.extend(
                offsets
                    .into_iter()
                    .filter(|offset| asked.contains(offset))
                    .map(|offset| {
                        (
                            query.assignee,
                            SlotPoint {
                                ledger: query.ledger,
                                slot_index: query.slot_index,
                                offset,
                            },
                        )
                    }),
            );
        }
        attestations
    }
}

impl ReplicaCoverage for HttpReplicaCoverage {
    fn released(&self, candidates: &[CoverageCandidate]) -> BTreeSet<(DataRoot, TxChunkOffset)> {
        if candidates.is_empty() {
            return BTreeSet::new();
        }
        if tokio::runtime::Handle::try_current().is_ok() {
            warn!("Replica coverage called from a Tokio runtime; keeping every candidate");
            return BTreeSet::new();
        }
        let slots = self.slots_of(candidates);
        // A slot the snapshot does not know counts as live: keep, do not guess.
        let live = |point: &SlotPoint| {
            slots
                .get(&(point.ledger, point.slot_index))
                .is_none_or(|slot| slot.as_ref().is_none_or(|slot| !slot.expired))
        };
        let queries = self.queries(candidates, &slots, live);
        let witnessed = remote_witnesses(&self.gather(queries), self.local_miner);
        candidates
            .iter()
            .filter(|candidate| chunk_released(&candidate.points, live, &witnessed))
            .map(|candidate| (candidate.data_root, candidate.tx_offset))
            .collect()
    }
}

/// How often one held offset may be announced: the receivers' claim
/// lifetime (`irys_p2p` `ChunkAnnounceLimits::claim_ttl`).
pub(super) const REANNOUNCE_INTERVAL: Duration = Duration::from_secs(300);
/// Most offsets one pass announces: the receivers' per-announcer claim
/// limit (`ChunkAnnounceLimits::max_per_peer`).
pub(super) const MAX_REANNOUNCED_PER_PASS: usize = 1_000;
/// Least time between two `HoldOnly` passes.
pub(super) const HOLD_ONLY_INTERVAL: Duration = Duration::from_secs(30);

/// When each held offset was last announced.
#[derive(Debug)]
pub struct ReannounceSchedule {
    interval: Duration,
    max_per_pass: usize,
    last: BTreeMap<(DataRoot, TxChunkOffset), Instant>,
}

impl Default for ReannounceSchedule {
    fn default() -> Self {
        Self::new(REANNOUNCE_INTERVAL, MAX_REANNOUNCED_PER_PASS)
    }
}

impl ReannounceSchedule {
    pub(super) fn new(interval: Duration, max_per_pass: usize) -> Self {
        Self {
            interval,
            max_per_pass,
            last: BTreeMap::new(),
        }
    }

    /// Offsets of `held` to announce at `now`, recorded as announced. None
    /// announced within `interval`; never-announced first, then the
    /// longest-unannounced; at most `max_per_pass`. A pass scans only part
    /// of the cache, so omitted offsets retain their timestamps until the
    /// interval expires.
    pub(super) fn due(
        &mut self,
        held: &BTreeSet<(DataRoot, TxChunkOffset)>,
        now: Instant,
    ) -> Vec<(DataRoot, TxChunkOffset)> {
        self.last.retain(|key, at| {
            held.contains(key) || now.saturating_duration_since(*at) < self.interval
        });
        let mut eligible: Vec<(Option<Instant>, (DataRoot, TxChunkOffset))> = held
            .iter()
            .filter_map(|key| match self.last.get(key) {
                Some(at) if now.saturating_duration_since(*at) < self.interval => None,
                last => Some((last.copied(), *key)),
            })
            .collect();
        eligible.sort_unstable();
        eligible.truncate(self.max_per_pass);
        for (_, key) in &eligible {
            self.last.insert(*key, now);
        }
        eligible.into_iter().map(|(_, key)| key).collect()
    }
}

/// What one capacity-prune pass may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassMode {
    /// Above the cache threshold: delete what is durable here or released
    /// by a replica, and re-announce what is held.
    Reclaim,
    /// Below the threshold: classify and re-announce; delete nothing.
    HoldOnly,
}

/// Mode for a pass at `now`, or `None` to skip the body pass. Below the
/// threshold a `HoldOnly` pass runs at most once per `hold_only_interval`.
pub(super) fn pass_mode(
    above_threshold: bool,
    last_hold_only: Option<Instant>,
    now: Instant,
    hold_only_interval: Duration,
) -> Option<PassMode> {
    if above_threshold {
        return Some(PassMode::Reclaim);
    }
    match last_hold_only {
        Some(at) if now.saturating_duration_since(at) < hold_only_interval => None,
        _ => Some(PassMode::HoldOnly),
    }
}

/// The cache task's custody collaborators. Each prune pass runs on a clone
/// of the task, so every clone shares the schedule and the cadence clock.
#[derive(Debug, Clone)]
pub struct ReplicaCustody {
    coverage: Arc<dyn ReplicaCoverage>,
    reannounce: Arc<Mutex<ReannounceSchedule>>,
    last_hold_only: Arc<Mutex<Option<Instant>>>,
}

impl ReplicaCustody {
    pub fn new(coverage: Arc<dyn ReplicaCoverage>) -> Self {
        Self {
            coverage,
            reannounce: Arc::default(),
            last_hold_only: Arc::default(),
        }
    }

    pub(super) fn released(
        &self,
        candidates: &[CoverageCandidate],
    ) -> BTreeSet<(DataRoot, TxChunkOffset)> {
        self.coverage.released(candidates)
    }

    pub(super) fn due_reannouncements(
        &self,
        held: &BTreeSet<(DataRoot, TxChunkOffset)>,
        now: Instant,
    ) -> Vec<(DataRoot, TxChunkOffset)> {
        self.reannounce
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .due(held, now)
    }

    /// Mode for a pass at `now`. A `HoldOnly` answer records `now`.
    pub(super) fn next_pass_mode(&self, above_threshold: bool, now: Instant) -> Option<PassMode> {
        let mut last = self
            .last_hold_only
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mode = pass_mode(above_threshold, *last, now, HOLD_ONLY_INTERVAL);
        if mode == Some(PassMode::HoldOnly) {
            *last = Some(now);
        }
        mode
    }

    /// Releases every candidate: the pass behaves as it did before custody.
    #[cfg(test)]
    pub(crate) fn all_replicated() -> Self {
        Self::new(Arc::new(stubs::AllReplicated))
    }
}

/// Coverage stubs for tests of the prune pass.
#[cfg(test)]
pub(crate) mod stubs {
    use super::*;

    #[derive(Debug)]
    pub(crate) struct AllReplicated;

    impl ReplicaCoverage for AllReplicated {
        fn released(
            &self,
            candidates: &[CoverageCandidate],
        ) -> BTreeSet<(DataRoot, TxChunkOffset)> {
            candidates
                .iter()
                .map(|candidate| (candidate.data_root, candidate.tx_offset))
                .collect()
        }
    }

    /// Releases only `keys`, and records every call's candidates.
    #[derive(Debug, Default)]
    pub(crate) struct ReleasedOnly {
        pub(crate) keys: Mutex<BTreeSet<(DataRoot, TxChunkOffset)>>,
        pub(crate) calls: Mutex<Vec<Vec<CoverageCandidate>>>,
    }

    impl ReplicaCoverage for ReleasedOnly {
        fn released(
            &self,
            candidates: &[CoverageCandidate],
        ) -> BTreeSet<(DataRoot, TxChunkOffset)> {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(candidates.to_vec());
            let keys = self
                .keys
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            candidates
                .iter()
                .map(|candidate| (candidate.data_root, candidate.tx_offset))
                .filter(|key| keys.contains(key))
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::LedgerChunkOffset;

    fn point(slot_index: usize, offset: u32) -> SlotPoint {
        SlotPoint {
            ledger: DataLedger::ThirtyDay,
            slot_index,
            offset,
        }
    }

    fn span(ledger: DataLedger, start: u64, chunk_count: u64) -> TxLedgerSpan {
        TxLedgerSpan {
            ledger,
            start: LedgerChunkOffset::from(start),
            chunk_count,
        }
    }

    #[test]
    fn slot_points_place_an_offset_in_each_span_that_reaches_it() {
        let spans = [
            span(DataLedger::ThirtyDay, 18, 5),
            // Too short to reach tx offset 3.
            span(DataLedger::ThirtyDay, 40, 3),
            // A duplicate span gives the same position once.
            span(DataLedger::ThirtyDay, 18, 5),
            // `start + 3` does not fit in a ledger offset.
            span(DataLedger::ThirtyDay, u64::MAX - 1, 5),
        ];
        assert_eq!(
            slot_points(&spans, TxChunkOffset::from(3), 10),
            vec![point(2, 1)],
            "ledger offset 21 is slot 2, partition offset 1"
        );
    }

    #[test]
    fn slot_points_give_one_position_per_ledger() {
        let spans = [
            span(DataLedger::Submit, 0, 4),
            span(DataLedger::Publish, 7, 4),
        ];
        let mut expected = vec![
            SlotPoint {
                ledger: DataLedger::Submit,
                slot_index: 0,
                offset: 2,
            },
            SlotPoint {
                ledger: DataLedger::Publish,
                slot_index: 0,
                offset: 9,
            },
        ];
        expected.sort_unstable();
        assert_eq!(slot_points(&spans, TxChunkOffset::from(2), 10), expected);
    }

    #[test]
    fn a_span_touches_every_slot_it_covers() {
        let live_slot_one = |ledger: DataLedger, slot_index: usize| {
            ledger == DataLedger::ThirtyDay && slot_index == 1
        };
        let touches = |spans: &[TxLedgerSpan]| spans_touch_live_slot(spans, 10, &live_slot_one);

        // Chunks 8..=11 straddle slots 0 and 1.
        assert!(touches(&[span(DataLedger::ThirtyDay, 8, 4)]));
        // Chunks 0..=9 stay in slot 0.
        assert!(!touches(&[span(DataLedger::ThirtyDay, 0, 10)]));
        // Slot 1 of another ledger is not this ledger's slot.
        assert!(!touches(&[span(DataLedger::Submit, 10, 1)]));
        // A zero-chunk span covers no slot.
        assert!(!touches(&[span(DataLedger::ThirtyDay, 10, 0)]));
        assert!(!touches(&[]));
        // An end that does not fit is unknown, and unknown keeps.
        assert!(touches(&[span(DataLedger::ThirtyDay, u64::MAX - 1, 5)]));
        // So is a partition size of zero.
        assert!(spans_touch_live_slot(
            &[span(DataLedger::Submit, 0, 1)],
            0,
            &|_, _| false
        ));
    }

    #[test]
    fn the_release_rule_needs_a_remote_witness_for_every_live_position() {
        let local = IrysAddress::repeat_byte(0);
        let remote = IrysAddress::repeat_byte(1);
        let live_slot_zero = |p: &SlotPoint| p.slot_index == 0;
        let witnessed = |answers: &[(IrysAddress, SlotPoint)]| remote_witnesses(answers, local);

        assert!(
            !chunk_released(&[], |_| true, &BTreeSet::new()),
            "a body with no position is kept"
        );
        assert!(
            chunk_released(&[point(1, 0)], live_slot_zero, &BTreeSet::new()),
            "a body whose every slot has expired is released"
        );
        assert!(chunk_released(
            &[point(0, 4)],
            live_slot_zero,
            &witnessed(&[(remote, point(0, 4))])
        ));
        assert!(
            !chunk_released(
                &[point(0, 4)],
                live_slot_zero,
                &witnessed(&[(local, point(0, 4))])
            ),
            "the local node never witnesses its own custody"
        );
        assert!(
            !chunk_released(
                &[point(0, 4), point(0, 5)],
                |_| true,
                &witnessed(&[(remote, point(0, 4))])
            ),
            "every live position needs a witness"
        );
    }

    proptest::proptest! {
        /// Released exactly when the body has a position and every live
        /// position has an attestation from an assignee other than the
        /// local node; any other assignee's silence does not block it.
        #[test]
        fn a_body_is_released_iff_every_live_position_has_a_remote_witness(
            positions in proptest::collection::btree_set((0..2_usize, 0..4_u32), 0..5),
            expired in proptest::collection::btree_set(0..2_usize, 0..3),
            answers in proptest::collection::vec((0..3_u8, 0..2_usize, 0..4_u32), 0..12),
        ) {
            let local = IrysAddress::repeat_byte(0);
            let points: Vec<SlotPoint> =
                positions.iter().map(|(slot, offset)| point(*slot, *offset)).collect();
            let attestations: Vec<(IrysAddress, SlotPoint)> = answers
                .iter()
                .map(|(who, slot, offset)| (IrysAddress::repeat_byte(*who), point(*slot, *offset)))
                .collect();
            let live = |p: &SlotPoint| !expired.contains(&p.slot_index);
            let expected = !points.is_empty()
                && points.iter().filter(|p| live(p)).all(|p| {
                    attestations
                        .iter()
                        .any(|(who, attested)| *who != local && attested == p)
                });

            let witnessed = remote_witnesses(&attestations, local);
            proptest::prop_assert_eq!(chunk_released(&points, live, &witnessed), expected);
        }
    }
    use crate::chunk_fetcher::MockChunkFetcher;
    use std::collections::BTreeMap;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    const LOCAL: u8 = 0;

    fn miner(byte: u8) -> IrysAddress {
        IrysAddress::repeat_byte(byte)
    }

    fn addr(byte: u8) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, byte], 9000))
    }

    #[derive(Debug, Default)]
    struct MapTopology {
        slots: BTreeMap<(DataLedger, usize), SlotCustodians>,
        addrs: BTreeMap<IrysAddress, SocketAddr>,
    }

    impl CustodyTopology for MapTopology {
        fn slot(&self, ledger: DataLedger, slot_index: usize) -> Option<SlotCustodians> {
            self.slots.get(&(ledger, slot_index)).cloned()
        }

        fn api_addr(&self, miner: &IrysAddress) -> Option<SocketAddr> {
            self.addrs.get(miner).copied()
        }
    }

    /// ThirtyDay slot 0, expired or not, assigned to `assignees`; every
    /// byte in `known` has an API address.
    fn topology(expired: bool, assignees: &[u8], known: &[u8]) -> MapTopology {
        MapTopology {
            slots: BTreeMap::from([(
                (DataLedger::ThirtyDay, 0),
                SlotCustodians {
                    expired,
                    assignees: assignees.iter().map(|byte| miner(*byte)).collect(),
                },
            )]),
            addrs: known
                .iter()
                .map(|byte| (miner(*byte), addr(*byte)))
                .collect(),
        }
    }

    fn quick_limits() -> CoverageLimits {
        CoverageLimits {
            request_timeout: Duration::from_millis(200),
            pass_deadline: Duration::from_secs(5),
            ..CoverageLimits::default()
        }
    }

    /// A runtime for the fetch futures, and a coverage over `fetcher`.
    /// Tests call `released` from the test thread, outside the runtime,
    /// as the prune thread does.
    fn coverage_over(
        topology: MapTopology,
        fetcher: &Arc<MockChunkFetcher>,
        limits: CoverageLimits,
    ) -> (tokio::runtime::Runtime, HttpReplicaCoverage) {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("runtime");
        let fetchers: BTreeMap<DataLedger, Arc<dyn ChunkFetcher>> = BTreeMap::from([(
            DataLedger::ThirtyDay,
            Arc::clone(fetcher) as Arc<dyn ChunkFetcher>,
        )]);
        let coverage = HttpReplicaCoverage::new(
            Arc::new(topology),
            miner(LOCAL),
            fetchers,
            limits,
            runtime.handle().clone(),
        );
        (runtime, coverage)
    }

    fn candidate(root: u8, tx_offset: u32, points: &[SlotPoint]) -> CoverageCandidate {
        CoverageCandidate {
            data_root: DataRoot::repeat_byte(root),
            tx_offset: TxChunkOffset::from(tx_offset),
            points: points.to_vec(),
        }
    }

    fn key(root: u8, tx_offset: u32) -> (DataRoot, TxChunkOffset) {
        (DataRoot::repeat_byte(root), TxChunkOffset::from(tx_offset))
    }

    #[test]
    fn one_query_per_assignee_and_slot_covers_many_roots() {
        let fetcher = Arc::new(
            MockChunkFetcher::new(
                usize::try_from(DataLedger::ThirtyDay).expect("ledger id fits usize"),
            )
            .with_persisted(addr(1), vec![1, 2, 5])
            // The local node would attest anything; it must not be asked.
            .with_persisted(addr(LOCAL), (0..10).collect()),
        );
        let (_runtime, coverage) = coverage_over(
            topology(false, &[LOCAL, 1], &[LOCAL, 1]),
            &fetcher,
            quick_limits(),
        );
        let candidates = [
            candidate(0xA, 0, &[point(0, 1)]),
            candidate(0xB, 0, &[point(0, 2)]),
            candidate(0xC, 3, &[point(0, 5)]),
            candidate(0xD, 0, &[point(0, 6)]),
        ];

        let released = coverage.released(&candidates);

        assert_eq!(
            released,
            BTreeSet::from([key(0xA, 0), key(0xB, 0), key(0xC, 3)])
        );
        let log = fetcher.persisted_request_log.read().unwrap();
        assert_eq!(log.len(), 1, "one batched request for the slot");
        assert_eq!(log[0].2, addr(1));
        assert_eq!(log[0].1.len(), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn truncated_rounds_reach_each_batch_on_a_healthy_later_assignee() -> eyre::Result<()> {
        let fetcher = MockChunkFetcher::new(usize::try_from(DataLedger::ThirtyDay)?)
            .with_persisted(addr(1), vec![1, 2])
            .with_persisted(addr(2), vec![1, 2]);
        fetcher
            .persisted_delays
            .write()
            .map_err(|_| eyre::eyre!("mock delays lock poisoned"))?
            .insert(addr(1), Duration::from_secs(60));
        let fetchers: BTreeMap<DataLedger, Arc<dyn ChunkFetcher>> = BTreeMap::from([(
            DataLedger::ThirtyDay,
            Arc::new(fetcher) as Arc<dyn ChunkFetcher>,
        )]);
        let coverage = HttpReplicaCoverage::new(
            Arc::new(topology(false, &[1, 2], &[1, 2])),
            miner(LOCAL),
            fetchers,
            CoverageLimits {
                request_timeout: Duration::from_secs(10),
                pass_deadline: Duration::from_millis(100),
                max_in_flight: 1,
                max_entries_per_request: 1,
            },
            tokio::runtime::Handle::current(),
        );
        let candidates = [
            candidate(0xA, 0, &[point(0, 1)]),
            candidate(0xA, 1, &[point(0, 2)]),
        ];

        for expected in [
            vec![],
            vec![(miner(2), point(0, 1))],
            vec![(miner(2), point(0, 2))],
        ] {
            let slots = coverage.slots_of(&candidates);
            let queries = coverage.queries(&candidates, &slots, |_| true);
            assert_eq!(coverage.gather_async(queries).await, expected);
        }
        Ok(())
    }

    /// A timed-out peer, a peer without the route (404), a peer with no
    /// known address and a peer that answers without the offset all
    /// attest nothing. One attesting peer then suffices.
    #[test]
    fn silence_errors_and_strangers_attest_nothing_and_one_witness_suffices() {
        let fetcher = Arc::new(
            MockChunkFetcher::new(
                usize::try_from(DataLedger::ThirtyDay).expect("ledger id fits usize"),
            )
            .with_persisted(addr(1), vec![1])
            .with_persisted(addr(4), vec![]),
        );
        fetcher
            .persisted_delays
            .write()
            .unwrap()
            .insert(addr(1), Duration::from_secs(2));
        let target = [candidate(0xA, 0, &[point(0, 1)])];

        let (_runtime, silent) = coverage_over(
            topology(false, &[1, 2, 3, 4], &[1, 2, 4]),
            &fetcher,
            quick_limits(),
        );
        assert!(silent.released(&target).is_empty());

        let fetcher = Arc::new(
            MockChunkFetcher::new(
                usize::try_from(DataLedger::ThirtyDay).expect("ledger id fits usize"),
            )
            .with_persisted(addr(4), vec![])
            .with_persisted(addr(5), vec![1]),
        );
        let (_runtime, one_witness) = coverage_over(
            topology(false, &[2, 3, 4, 5], &[2, 4, 5]),
            &fetcher,
            quick_limits(),
        );
        assert_eq!(one_witness.released(&target), BTreeSet::from([key(0xA, 0)]));
    }

    #[test]
    fn the_pass_deadline_keeps_what_has_not_answered() {
        let fetcher = Arc::new(
            MockChunkFetcher::new(
                usize::try_from(DataLedger::ThirtyDay).expect("ledger id fits usize"),
            )
            .with_persisted(addr(1), vec![1]),
        );
        fetcher
            .persisted_delays
            .write()
            .unwrap()
            .insert(addr(1), Duration::from_secs(10));
        let limits = CoverageLimits {
            request_timeout: Duration::from_secs(30),
            pass_deadline: Duration::from_millis(200),
            ..CoverageLimits::default()
        };
        let (_runtime, coverage) = coverage_over(topology(false, &[1], &[1]), &fetcher, limits);

        let started = Instant::now();
        let released = coverage.released(&[candidate(0xA, 0, &[point(0, 1)])]);

        assert!(released.is_empty());
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the deadline, not the slow peer, ends the pass"
        );
    }

    /// Positions only in expired slots need no witness; a slot the
    /// topology does not know counts as live and has no assignee to ask;
    /// a body with no position is kept.
    #[test]
    fn expired_unknown_and_unplaced_positions() {
        let fetcher = Arc::new(MockChunkFetcher::new(
            usize::try_from(DataLedger::ThirtyDay).expect("ledger id fits usize"),
        ));
        let (_runtime, coverage) =
            coverage_over(topology(true, &[1], &[1]), &fetcher, quick_limits());

        let released = coverage.released(&[
            candidate(0xA, 0, &[point(0, 1)]),
            candidate(0xB, 0, &[point(7, 1)]),
            candidate(0xC, 0, &[]),
        ]);

        assert_eq!(released, BTreeSet::from([key(0xA, 0)]));
        assert!(fetcher.persisted_request_log.read().unwrap().is_empty());
    }

    #[test]
    fn requests_respect_the_entry_limit() {
        let fetcher = Arc::new(
            MockChunkFetcher::new(
                usize::try_from(DataLedger::ThirtyDay).expect("ledger id fits usize"),
            )
            .with_persisted(addr(1), (0..5).collect()),
        );
        let limits = CoverageLimits {
            max_entries_per_request: 2,
            ..quick_limits()
        };
        let (_runtime, coverage) = coverage_over(topology(false, &[1], &[1]), &fetcher, limits);
        let candidates: Vec<CoverageCandidate> = (0..5_u32)
            .map(|offset| candidate(0xA, offset, &[point(0, offset)]))
            .collect();

        assert_eq!(coverage.released(&candidates).len(), 5);
        let log = fetcher.persisted_request_log.read().unwrap();
        let sizes: Vec<usize> = log.iter().map(|(_, entries, _)| entries.len()).collect();
        assert_eq!(log.len(), 3, "{sizes:?}");
        assert!(sizes.iter().all(|size| *size <= 2), "{sizes:?}");
    }

    #[test]
    fn a_position_claimed_by_two_chunks_is_not_asked_about() {
        let fetcher = Arc::new(
            MockChunkFetcher::new(
                usize::try_from(DataLedger::ThirtyDay).expect("ledger id fits usize"),
            )
            .with_persisted(addr(1), vec![1]),
        );
        let (_runtime, coverage) =
            coverage_over(topology(false, &[1], &[1]), &fetcher, quick_limits());

        let released = coverage.released(&[
            candidate(0xA, 0, &[point(0, 1)]),
            candidate(0xB, 0, &[point(0, 1)]),
        ]);

        assert!(released.is_empty());
    }

    #[test]
    fn coverage_inside_a_runtime_releases_nothing() {
        let fetcher = Arc::new(
            MockChunkFetcher::new(
                usize::try_from(DataLedger::ThirtyDay).expect("ledger id fits usize"),
            )
            .with_persisted(addr(1), vec![1]),
        );
        let (runtime, coverage) =
            coverage_over(topology(false, &[1], &[1]), &fetcher, quick_limits());

        let released =
            runtime.block_on(async { coverage.released(&[candidate(0xA, 0, &[point(0, 1)])]) });

        assert!(
            released.is_empty(),
            "block_on would panic here; keep instead"
        );
    }
    #[test]
    fn the_schedule_announces_each_held_offset_once_per_interval() {
        let mut schedule = ReannounceSchedule::new(Duration::from_secs(300), 1_000);
        let held = BTreeSet::from([key(0xA, 0), key(0xA, 1)]);
        let start = Instant::now();

        assert_eq!(schedule.due(&held, start), vec![key(0xA, 0), key(0xA, 1)]);
        assert!(
            schedule
                .due(&held, start + Duration::from_secs(299))
                .is_empty()
        );
        assert_eq!(
            schedule.due(&held, start + Duration::from_secs(300)).len(),
            2
        );
    }

    #[test]
    fn the_schedule_caps_a_pass_and_rotates_to_unannounced_offsets() {
        let mut schedule = ReannounceSchedule::new(Duration::from_secs(300), 2);
        let held: BTreeSet<_> = (0..5).map(|offset| key(0xA, offset)).collect();
        let start = Instant::now();

        assert_eq!(schedule.due(&held, start), vec![key(0xA, 0), key(0xA, 1)]);
        assert_eq!(
            schedule.due(&held, start + Duration::from_secs(1)),
            vec![key(0xA, 2), key(0xA, 3)]
        );
        assert_eq!(
            schedule.due(&held, start + Duration::from_secs(400)),
            vec![key(0xA, 4), key(0xA, 0)],
            "the never-announced offset goes before the oldest announcement"
        );
    }

    #[test]
    fn the_schedule_retains_recent_offsets_missing_from_a_scan() {
        let mut schedule = ReannounceSchedule::new(Duration::from_secs(300), 10);
        let start = Instant::now();
        let first = BTreeSet::from([key(0xA, 0)]);
        let second = BTreeSet::from([key(0xB, 0)]);

        assert_eq!(schedule.due(&first, start), vec![key(0xA, 0)]);
        assert_eq!(
            schedule.due(&second, start + Duration::from_secs(1)),
            vec![key(0xB, 0)]
        );
        assert!(
            schedule
                .due(&first, start + Duration::from_secs(2))
                .is_empty()
        );
        assert_eq!(
            schedule.due(&first, start + Duration::from_secs(300)),
            vec![key(0xA, 0)]
        );
        schedule.due(&BTreeSet::new(), start + Duration::from_secs(600));
        assert!(
            schedule.last.is_empty(),
            "expired timestamps absent from the scan are reclaimed"
        );
    }

    #[test]
    fn pass_mode_reclaims_above_the_threshold_and_holds_at_most_every_interval() {
        let start = Instant::now();
        let interval = Duration::from_secs(30);

        assert_eq!(
            pass_mode(true, Some(start), start, interval),
            Some(PassMode::Reclaim)
        );
        assert_eq!(
            pass_mode(false, None, start, interval),
            Some(PassMode::HoldOnly)
        );
        assert_eq!(
            pass_mode(
                false,
                Some(start),
                start + Duration::from_secs(29),
                interval
            ),
            None
        );
        assert_eq!(
            pass_mode(false, Some(start), start + interval, interval),
            Some(PassMode::HoldOnly)
        );
    }

    #[test]
    fn custody_shares_the_hold_only_clock_across_clones() {
        let custody = ReplicaCustody::all_replicated();
        let clone = custody.clone();
        let start = Instant::now();

        assert_eq!(
            custody.next_pass_mode(false, start),
            Some(PassMode::HoldOnly)
        );
        assert_eq!(
            clone.next_pass_mode(false, start + Duration::from_secs(1)),
            None,
            "a clone sees the pass the original recorded"
        );
        assert_eq!(
            clone.next_pass_mode(true, start + Duration::from_secs(1)),
            Some(PassMode::Reclaim)
        );
    }
}
