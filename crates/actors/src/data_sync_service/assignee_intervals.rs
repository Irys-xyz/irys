//! What each assignee of a slot reports holding as Data.
//!
//! Data sync walks local Entropy one offset at a time and asks assignees for
//! each body. Where every assignee is missing a long run, that walk spends one
//! 404 per offset before it reaches a range a peer can serve. These hints let
//! the scan skip local Entropy that no assignee claims.
//!
//! Hints only steer scheduling. A fetched body is still checked against its
//! merkle proof, so a wrong or stale hint can delay a fetch but cannot store
//! bad data. Coverage is unknown unless every assignee gave a usable answer,
//! and unknown always means "probe as before".

use crate::chunk_fetcher::{ChunkFetchError, ChunkFetcher, DataIntervalsPage, IntervalsCoverage};
use irys_types::IrysAddress;
use std::{
    collections::HashMap,
    net::SocketAddr,
    time::{Duration, Instant},
};

/// Chunks requested in one window ahead of the scan cursor when an answer has
/// not shown how dense the peer's map is.
pub(crate) const HINT_WINDOW_CHUNKS: u32 = 1 << 22;

/// Smallest window. A map dense enough to fill every page of a window this
/// size stays unknown past it, which only costs per-offset probing.
const HINT_MIN_WINDOW_CHUNKS: u32 = 1 << 14;

/// A known answer older than this is asked for again, so a body a peer gains
/// later is picked up.
const HINT_REFRESH: Duration = Duration::from_secs(60);

/// Shortest gap between two fetches for one peer when the scan has moved near
/// the edge of its window.
const HINT_MIN_REFETCH: Duration = Duration::from_secs(5);

/// Wait after a failed or unusable answer before asking that peer again.
const HINT_RETRY_AFTER_FAILURE: Duration = Duration::from_secs(60);

/// A fetch with no result after this is dropped, and the previous answer, if
/// any, is used again. Covers every page of a window at [`HINT_PAGE_TIMEOUT`].
const HINT_FETCH_TIMEOUT: Duration = Duration::from_secs(300);

/// Per-page request timeout. An older peer sends its whole list in one page.
pub(crate) const HINT_PAGE_TIMEOUT: Duration = Duration::from_secs(30);

/// Most pages read for one window. The window halves when this is not enough.
pub(crate) const HINT_MAX_PAGES: usize = 16;

/// Offsets this close to the write head are always probed. A peer's map lags
/// chunks that migrated after it answered.
pub(crate) const HINT_FRONTIER_MARGIN: u32 = 16_384;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Coverage {
    /// At least one assignee holds this span as Data.
    Claimed,
    /// Every assignee answered for this span and none holds it.
    Unclaimed,
    /// Some assignee has not answered for this span.
    Unknown,
}

#[derive(Debug, Clone)]
struct Known {
    /// Inclusive span this answer is complete for.
    covers: (u32, u32),
    /// Data runs inside `covers`, ascending, disjoint.
    data: Vec<(u32, u32)>,
    fetched_at: Instant,
    /// A later window could answer below `covers.0`.
    extendable_low: bool,
    /// A later window could answer above `covers.1`. False for a full list,
    /// and when the frontier margin cut the answer: asking again would be cut
    /// the same way.
    extendable_high: bool,
}

impl Known {
    fn holds(&self, offset: u32) -> bool {
        let index = self.data.partition_point(|run| run.1 < offset);
        self.data.get(index).is_some_and(|run| run.0 <= offset)
    }
}

#[derive(Debug)]
enum PeerHint {
    /// No usable answer. Asked again at `retry_at`.
    Unknown {
        retry_at: Instant,
    },
    /// A fetch is in flight. `prior` keeps answering until it lands.
    Fetching {
        started: Instant,
        prior: Option<Known>,
    },
    Known(Known),
}

#[derive(Debug)]
struct PeerState {
    hint: PeerHint,
    /// Window size for this peer's next fetch. Halves when the pages run out
    /// before the window does.
    window: u32,
}

/// One planned fetch: read `start..=end` from `peer`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowFetch {
    pub peer: IrysAddress,
    pub start: u32,
    pub end: u32,
}

#[derive(Debug)]
pub(crate) struct AssigneeIntervals {
    peers: HashMap<IrysAddress, PeerState>,
    frontier_margin: u32,
}

impl AssigneeIntervals {
    pub(crate) fn new(frontier_margin: u32) -> Self {
        Self {
            peers: HashMap::new(),
            frontier_margin,
        }
    }

    fn known(&self, peer: &IrysAddress) -> Option<&Known> {
        match &self.peers.get(peer)?.hint {
            PeerHint::Known(known)
            | PeerHint::Fetching {
                prior: Some(known), ..
            } => Some(known),
            PeerHint::Unknown { .. } | PeerHint::Fetching { prior: None, .. } => None,
        }
    }

    /// Windows to fetch this tick. Each returned peer is marked in flight.
    /// Peers that are no longer assignees are forgotten.
    pub(crate) fn plan_fetches(
        &mut self,
        assignees: &[IrysAddress],
        cursor: u32,
        max_offset: u32,
        reverse: bool,
        now: Instant,
    ) -> Vec<WindowFetch> {
        self.peers.retain(|peer, _| assignees.contains(peer));
        let cursor = cursor.min(max_offset);
        let mut planned = Vec::new();
        for &peer in assignees {
            let state = self.peers.entry(peer).or_insert(PeerState {
                hint: PeerHint::Unknown { retry_at: now },
                window: HINT_WINDOW_CHUNKS,
            });
            let due = match &mut state.hint {
                PeerHint::Unknown { retry_at } => *retry_at <= now,
                PeerHint::Fetching { started, prior } => {
                    if now.saturating_duration_since(*started) >= HINT_FETCH_TIMEOUT {
                        state.hint = match prior.take() {
                            Some(known) => PeerHint::Known(known),
                            None => PeerHint::Unknown {
                                retry_at: now + HINT_RETRY_AFTER_FAILURE,
                            },
                        };
                    }
                    false
                }
                PeerHint::Known(known) => {
                    let age = now.saturating_duration_since(known.fetched_at);
                    age >= HINT_REFRESH
                        || (age >= HINT_MIN_REFETCH
                            && near_window_edge(known, cursor, state.window, reverse))
                }
            };
            if !due {
                continue;
            }
            let (start, end) = window_at(cursor, max_offset, state.window, reverse);
            let prior =
                match std::mem::replace(&mut state.hint, PeerHint::Unknown { retry_at: now }) {
                    PeerHint::Known(known) => Some(known),
                    PeerHint::Fetching { prior, .. } => prior,
                    PeerHint::Unknown { .. } => None,
                };
            state.hint = PeerHint::Fetching {
                started: now,
                prior,
            };
            planned.push(WindowFetch { peer, start, end });
        }
        planned
    }

    /// Record the answer to a fetch planned by [`Self::plan_fetches`].
    /// `head` is the partition-relative write head when it was planned.
    pub(crate) fn on_fetched(
        &mut self,
        fetch: WindowFetch,
        head: u32,
        result: Result<DataIntervalsPage, ChunkFetchError>,
        now: Instant,
    ) {
        let Some(state) = self.peers.get_mut(&fetch.peer) else {
            return;
        };
        if !matches!(state.hint, PeerHint::Fetching { .. }) {
            // A late answer after a timeout already settled this peer.
            return;
        }
        if let Ok(page) = &result
            && let IntervalsCoverage::Window { end, .. } = page.coverage
        {
            state.window = if end < fetch.end {
                (state.window / 2).max(HINT_MIN_WINDOW_CHUNKS)
            } else {
                state.window.saturating_mul(2).min(HINT_WINDOW_CHUNKS)
            };
        }
        let known = result
            .ok()
            .and_then(|page| into_known(page, fetch.start, head, self.frontier_margin, now));
        state.hint = match known {
            Some(known) => PeerHint::Known(known),
            None => PeerHint::Unknown {
                retry_at: now + HINT_RETRY_AFTER_FAILURE,
            },
        };
    }

    /// Split `start..=end` by what `assignees` claim, ascending. Spans no
    /// answer covers are [`Coverage::Unknown`].
    pub(crate) fn segments(
        &self,
        assignees: &[IrysAddress],
        start: u32,
        end: u32,
    ) -> Vec<(u32, u32, Coverage)> {
        let unknown = vec![(start, end, Coverage::Unknown)];
        if assignees.is_empty() || start > end {
            return unknown;
        }
        let mut knowns = Vec::with_capacity(assignees.len());
        let (mut low, mut high) = (start, end);
        for peer in assignees {
            let Some(known) = self.known(peer) else {
                return unknown;
            };
            low = low.max(known.covers.0);
            high = high.min(known.covers.1);
            knowns.push(known);
        }
        if low > high {
            return unknown;
        }

        let mut claimed: Vec<(u32, u32)> = Vec::new();
        for known in knowns {
            let first = known.data.partition_point(|run| run.1 < low);
            for &(run_start, run_end) in &known.data[first..] {
                if run_start > high {
                    break;
                }
                claimed.push((run_start.max(low), run_end.min(high)));
            }
        }
        claimed.sort_unstable();

        let mut segments = Vec::new();
        if start < low {
            segments.push((start, low - 1, Coverage::Unknown));
        }
        // Next offset not yet assigned to a segment. `None` once past `high`.
        let mut next = Some(low);
        for (run_start, run_end) in claimed {
            let Some(position) = next else {
                break;
            };
            if run_end < position {
                continue;
            }
            if run_start > position {
                segments.push((position, run_start - 1, Coverage::Unclaimed));
            }
            let claimed_from = run_start.max(position);
            match segments.last_mut() {
                Some(last) if last.2 == Coverage::Claimed && last.1 + 1 == claimed_from => {
                    last.1 = run_end;
                }
                _ => segments.push((claimed_from, run_end, Coverage::Claimed)),
            }
            next = run_end.checked_add(1).filter(|offset| *offset <= high);
        }
        if let Some(position) = next {
            segments.push((position, high, Coverage::Unclaimed));
        }
        if high < end {
            segments.push((high + 1, end, Coverage::Unknown));
        }
        segments
    }

    /// True when `peer` answered for `offset` and does not hold it.
    pub(crate) fn excludes(&self, peer: &IrysAddress, offset: u32) -> bool {
        self.known(peer).is_some_and(|known| {
            known.covers.0 <= offset && offset <= known.covers.1 && !known.holds(offset)
        })
    }

    #[cfg(test)]
    pub(crate) fn insert_known(
        &mut self,
        peer: IrysAddress,
        covers: (u32, u32),
        data: Vec<(u32, u32)>,
        now: Instant,
    ) {
        self.peers.insert(
            peer,
            PeerState {
                hint: PeerHint::Known(Known {
                    covers,
                    data,
                    fetched_at: now,
                    extendable_low: false,
                    extendable_high: false,
                }),
                window: HINT_WINDOW_CHUNKS,
            },
        );
    }
}

/// The span to fetch for a scan at `cursor`: below it on a reverse scan,
/// above it otherwise.
fn window_at(cursor: u32, max_offset: u32, window: u32, reverse: bool) -> (u32, u32) {
    let span = window.max(1) - 1;
    if reverse {
        (cursor.saturating_sub(span), cursor)
    } else {
        (cursor, cursor.saturating_add(span).min(max_offset))
    }
}

/// True when a new window would answer where the scan is, or is about to be:
/// the cursor is past an edge that can be extended, or within a quarter window
/// of the extendable edge it is walking towards.
fn near_window_edge(known: &Known, cursor: u32, window: u32, reverse: bool) -> bool {
    let (low, high) = known.covers;
    if cursor < low {
        return known.extendable_low;
    }
    if cursor > high {
        return known.extendable_high;
    }
    let lead = window / 4;
    if reverse {
        known.extendable_low && cursor - low < lead
    } else {
        known.extendable_high && high - cursor < lead
    }
}

/// A usable answer, or `None` when it must leave the peer unknown: an empty
/// full list (which an unassigned older peer also returns), an unassigned
/// window, or a span entirely inside the frontier margin.
fn into_known(
    page: DataIntervalsPage,
    window_start: u32,
    head: u32,
    frontier_margin: u32,
    now: Instant,
) -> Option<Known> {
    let settled_end = head.checked_sub(frontier_margin)?;
    let (covers, extendable_low, extendable_high) = match page.coverage {
        IntervalsCoverage::FullList if page.intervals.is_empty() => return None,
        IntervalsCoverage::FullList => ((0, settled_end), false, false),
        IntervalsCoverage::Window {
            assigned: false, ..
        } => return None,
        IntervalsCoverage::Window { end, .. } => (
            (window_start, end.min(settled_end)),
            window_start > 0,
            end < settled_end,
        ),
    };
    if covers.0 > covers.1 {
        return None;
    }
    let data = page
        .intervals
        .into_iter()
        .filter(|run| run.1 >= covers.0 && run.0 <= covers.1)
        .map(|run| (run.0.max(covers.0), run.1.min(covers.1)))
        .collect();
    Some(Known {
        covers,
        data,
        fetched_at: now,
        extendable_low,
        extendable_high,
    })
}

/// Read `start..=end` from one peer, following windowed answers for up to
/// [`HINT_MAX_PAGES`] pages. A failure after the first page keeps what was
/// read; the window then ends where the last good page did.
pub(crate) async fn fetch_window(
    fetcher: &dyn ChunkFetcher,
    slot_index: usize,
    start: u32,
    end: u32,
    api_addr: SocketAddr,
    timeout: Duration,
) -> Result<DataIntervalsPage, ChunkFetchError> {
    let mut page = fetcher
        .fetch_data_intervals(slot_index, start, end, api_addr, timeout)
        .await?;
    for _ in 1..HINT_MAX_PAGES {
        let IntervalsCoverage::Window {
            end: covered,
            assigned: true,
        } = page.coverage
        else {
            break;
        };
        if covered >= end {
            break;
        }
        let Ok(next) = fetcher
            .fetch_data_intervals(slot_index, covered + 1, end, api_addr, timeout)
            .await
        else {
            break;
        };
        let IntervalsCoverage::Window {
            end: next_end,
            assigned: true,
        } = next.coverage
        else {
            break;
        };
        for run in next.intervals {
            match page.intervals.last_mut() {
                Some(last) if last.1.checked_add(1) == Some(run.0) => last.1 = run.1,
                _ => page.intervals.push(run),
            }
        }
        page.coverage = IntervalsCoverage::Window {
            end: next_end,
            assigned: true,
        };
    }
    Ok(page)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(byte: u8) -> IrysAddress {
        IrysAddress::from([byte; 20])
    }

    fn window(intervals: Vec<(u32, u32)>, end: u32, assigned: bool) -> DataIntervalsPage {
        DataIntervalsPage {
            intervals,
            coverage: IntervalsCoverage::Window { end, assigned },
        }
    }

    fn full(intervals: Vec<(u32, u32)>) -> DataIntervalsPage {
        DataIntervalsPage {
            intervals,
            coverage: IntervalsCoverage::FullList,
        }
    }

    /// Plan one fetch for `p` and answer it.
    fn answer(
        hints: &mut AssigneeIntervals,
        p: IrysAddress,
        head: u32,
        result: Result<DataIntervalsPage, ChunkFetchError>,
        now: Instant,
    ) {
        let planned = hints.plan_fetches(&[p], head, head, true, now);
        assert_eq!(planned.len(), 1, "a fetch is due");
        hints.on_fetched(planned[0], head, result, now);
    }

    #[test]
    fn no_answer_is_unknown() {
        let hints = AssigneeIntervals::new(0);
        assert_eq!(
            hints.segments(&[peer(1)], 0, 9),
            vec![(0, 9, Coverage::Unknown)]
        );
        assert_eq!(
            hints.segments(&[], 0, 9),
            vec![(0, 9, Coverage::Unknown)],
            "no assignee, nothing to skip for"
        );
    }

    #[test]
    fn a_full_list_splits_claimed_and_unclaimed_below_the_frontier() {
        let now = Instant::now();
        let mut hints = AssigneeIntervals::new(10);
        answer(
            &mut hints,
            peer(1),
            100,
            Ok(full(vec![(20, 29), (60, 95)])),
            now,
        );

        assert_eq!(
            hints.segments(&[peer(1)], 0, 100),
            vec![
                (0, 19, Coverage::Unclaimed),
                (20, 29, Coverage::Claimed),
                (30, 59, Coverage::Unclaimed),
                (60, 90, Coverage::Claimed),
                (91, 100, Coverage::Unknown),
            ],
            "the last ten offsets below the head stay unknown"
        );
    }

    #[test]
    fn an_empty_full_list_or_an_unassigned_window_stays_unknown() {
        let now = Instant::now();
        let mut hints = AssigneeIntervals::new(0);
        answer(&mut hints, peer(1), 100, Ok(full(vec![])), now);
        assert_eq!(
            hints.segments(&[peer(1)], 0, 50),
            vec![(0, 50, Coverage::Unknown)]
        );

        let mut hints = AssigneeIntervals::new(0);
        answer(
            &mut hints,
            peer(1),
            100,
            Ok(window(vec![], 100, false)),
            now,
        );
        assert_eq!(
            hints.segments(&[peer(1)], 0, 50),
            vec![(0, 50, Coverage::Unknown)]
        );
    }

    #[test]
    fn an_assigned_empty_window_is_unclaimed() {
        let now = Instant::now();
        let mut hints = AssigneeIntervals::new(0);
        let planned = hints.plan_fetches(&[peer(1)], 100, 100, true, now);
        hints.on_fetched(planned[0], 100, Ok(window(vec![], 100, true)), now);
        assert_eq!(
            hints.segments(&[peer(1)], 0, 100),
            vec![(0, 100, Coverage::Unclaimed)]
        );
    }

    #[test]
    fn assignees_union_their_data_inside_the_span_all_of_them_cover() {
        let now = Instant::now();
        let mut hints = AssigneeIntervals::new(0);
        hints.insert_known(peer(1), (0, 80), vec![(10, 19)], now);
        hints.insert_known(peer(2), (20, 100), vec![(15, 30), (50, 59)], now);

        assert_eq!(
            hints.segments(&[peer(1), peer(2)], 0, 100),
            vec![
                (0, 19, Coverage::Unknown),
                (20, 30, Coverage::Claimed),
                (31, 49, Coverage::Unclaimed),
                (50, 59, Coverage::Claimed),
                (60, 80, Coverage::Unclaimed),
                (81, 100, Coverage::Unknown),
            ]
        );
        assert_eq!(
            hints.segments(&[peer(1), peer(3)], 0, 100),
            vec![(0, 100, Coverage::Unknown)],
            "one silent assignee makes the whole span unknown"
        );
    }

    #[test]
    fn claimed_runs_that_touch_merge_and_overlaps_do_not_repeat() {
        let now = Instant::now();
        let mut hints = AssigneeIntervals::new(0);
        hints.insert_known(peer(1), (0, 50), vec![(10, 20)], now);
        hints.insert_known(peer(2), (0, 50), vec![(15, 25), (26, 30)], now);
        assert_eq!(
            hints.segments(&[peer(1), peer(2)], 0, 50),
            vec![
                (0, 9, Coverage::Unclaimed),
                (10, 30, Coverage::Claimed),
                (31, 50, Coverage::Unclaimed),
            ]
        );
    }

    #[test]
    fn a_claimed_run_at_u32_max_does_not_overflow() {
        let now = Instant::now();
        let mut hints = AssigneeIntervals::new(0);
        hints.insert_known(peer(1), (0, u32::MAX), vec![(u32::MAX - 1, u32::MAX)], now);
        assert_eq!(
            hints.segments(&[peer(1)], u32::MAX - 3, u32::MAX),
            vec![
                (u32::MAX - 3, u32::MAX - 2, Coverage::Unclaimed),
                (u32::MAX - 1, u32::MAX, Coverage::Claimed),
            ]
        );
    }

    #[test]
    fn excludes_only_offsets_a_peer_answered_for() {
        let now = Instant::now();
        let mut hints = AssigneeIntervals::new(0);
        hints.insert_known(peer(1), (10, 50), vec![(20, 29)], now);
        assert!(hints.excludes(&peer(1), 15));
        assert!(!hints.excludes(&peer(1), 25), "the peer holds it");
        assert!(!hints.excludes(&peer(1), 5), "outside the answer");
        assert!(!hints.excludes(&peer(2), 15), "no answer from this peer");
    }

    #[test]
    fn fetches_are_planned_once_refreshed_and_backed_off() {
        let now = Instant::now();
        let mut hints = AssigneeIntervals::new(0);
        let assignees = [peer(1)];

        let first = hints.plan_fetches(&assignees, u32::MAX, 1_000, true, now);
        assert_eq!(
            first,
            vec![WindowFetch {
                peer: peer(1),
                start: 0,
                end: 1_000
            }],
            "a reverse scan reads the window below the cursor"
        );
        assert!(
            hints
                .plan_fetches(&assignees, 1_000, 1_000, true, now)
                .is_empty(),
            "no second fetch while one is in flight"
        );

        hints.on_fetched(first[0], 1_000, Ok(full(vec![(0, 9)])), now);
        assert!(
            hints
                .plan_fetches(&assignees, 1_000, 1_000, true, now)
                .is_empty()
        );
        assert_eq!(
            hints
                .plan_fetches(&assignees, 1_000, 1_000, true, now + HINT_REFRESH)
                .len(),
            1,
            "an old answer is fetched again"
        );

        let mut failing = AssigneeIntervals::new(0);
        let planned = failing.plan_fetches(&assignees, 1_000, 1_000, true, now);
        failing.on_fetched(planned[0], 1_000, Err(ChunkFetchError::Timeout), now);
        assert!(
            failing
                .plan_fetches(&assignees, 1_000, 1_000, true, now + HINT_MIN_REFETCH)
                .is_empty(),
            "a failure backs off"
        );
        assert_eq!(
            failing
                .plan_fetches(
                    &assignees,
                    1_000,
                    1_000,
                    true,
                    now + HINT_RETRY_AFTER_FAILURE
                )
                .len(),
            1
        );
    }

    #[test]
    fn a_timed_out_fetch_keeps_the_previous_answer() {
        let now = Instant::now();
        let mut hints = AssigneeIntervals::new(0);
        hints.insert_known(peer(1), (0, 100), vec![(0, 9)], now);
        let later = now + HINT_REFRESH;
        assert_eq!(
            hints.plan_fetches(&[peer(1)], 100, 100, true, later).len(),
            1
        );
        assert_eq!(
            hints.segments(&[peer(1)], 0, 20)[0],
            (0, 9, Coverage::Claimed),
            "the old answer still applies while the refresh is in flight"
        );
        let expired = later + HINT_FETCH_TIMEOUT;
        assert!(
            hints
                .plan_fetches(&[peer(1)], 100, 100, true, expired)
                .is_empty()
        );
        assert_eq!(
            hints.segments(&[peer(1)], 0, 20)[0],
            (0, 9, Coverage::Claimed)
        );
    }

    #[test]
    fn a_cut_window_shrinks_and_the_scan_edge_triggers_a_refetch() {
        let now = Instant::now();
        let mut hints = AssigneeIntervals::new(0);
        let head = 10_000_000;
        let planned = hints.plan_fetches(&[peer(1)], head, head, true, now);
        let fetch = planned[0];
        assert_eq!(fetch.end - fetch.start + 1, HINT_WINDOW_CHUNKS);
        // The pages ran out halfway: complete only up to the midpoint.
        let mid = fetch.start + HINT_WINDOW_CHUNKS / 2;
        hints.on_fetched(fetch, head, Ok(window(vec![], mid, true)), now);

        let soon = now + HINT_MIN_REFETCH;
        let next = hints.plan_fetches(&[peer(1)], head, head, true, soon);
        assert_eq!(next.len(), 1, "the cursor is outside what was answered");
        assert_eq!(next[0].end - next[0].start + 1, HINT_WINDOW_CHUNKS / 2);
    }

    /// A reverse scan starts at the head, above what any answer covers. That
    /// must not refetch every few seconds: a new answer would be cut by the
    /// frontier margin the same way.
    #[test]
    fn the_frontier_margin_does_not_trigger_refetches() {
        let now = Instant::now();
        let soon = now + HINT_MIN_REFETCH;
        for page in [full(vec![(0, 9)]), window(vec![(0, 9)], 1_000, true)] {
            let mut hints = AssigneeIntervals::new(100);
            let planned = hints.plan_fetches(&[peer(1)], 1_000, 1_000, true, now);
            hints.on_fetched(planned[0], 1_000, Ok(page), now);
            assert!(
                hints
                    .plan_fetches(&[peer(1)], 1_000, 1_000, true, soon)
                    .is_empty(),
                "the cursor is in the frontier band, not past an extendable edge"
            );
        }
    }

    #[test]
    fn peers_that_leave_the_assignee_set_are_forgotten() {
        let now = Instant::now();
        let mut hints = AssigneeIntervals::new(0);
        hints.insert_known(peer(1), (0, 100), vec![], now);
        hints.plan_fetches(&[peer(2)], 100, 100, true, now);
        assert!(!hints.excludes(&peer(1), 50));
    }

    #[tokio::test]
    async fn fetch_window_follows_pages_and_merges_at_the_seam() {
        use crate::chunk_fetcher::MockChunkFetcher;

        #[derive(Debug)]
        struct Paged(MockChunkFetcher);
        #[async_trait::async_trait]
        impl ChunkFetcher for Paged {
            async fn fetch_chunk_by_ledger_offset(
                &self,
                offset: irys_types::LedgerChunkOffset,
                api: SocketAddr,
                timeout: Duration,
            ) -> Result<irys_types::ChunkFormat, ChunkFetchError> {
                self.0
                    .fetch_chunk_by_ledger_offset(offset, api, timeout)
                    .await
            }
            async fn fetch_chunk_by_data_root(
                &self,
                root: irys_types::DataRoot,
                tx_offset: irys_types::TxChunkOffset,
                api: SocketAddr,
                timeout: Duration,
            ) -> Result<irys_types::ChunkFormat, ChunkFetchError> {
                self.0
                    .fetch_chunk_by_data_root(root, tx_offset, api, timeout)
                    .await
            }
            async fn fetch_data_intervals(
                &self,
                _slot: usize,
                start: u32,
                _end: u32,
                _api: SocketAddr,
                _timeout: Duration,
            ) -> Result<DataIntervalsPage, ChunkFetchError> {
                Ok(match start {
                    0 => window(vec![(0, 4), (8, 9)], 9, true),
                    10 => window(vec![(10, 12)], 19, true),
                    _ => window(vec![(25, 29)], 29, true),
                })
            }
        }

        let fetcher = Paged(MockChunkFetcher::new(20));
        let api: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let page = fetch_window(&fetcher, 0, 0, 29, api, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(page.intervals, vec![(0, 4), (8, 12), (25, 29)]);
        assert_eq!(
            page.coverage,
            IntervalsCoverage::Window {
                end: 29,
                assigned: true
            }
        );
    }
}
