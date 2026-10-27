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
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

/// Source of [`WindowFetch::seq`]. Process-wide, so an orchestrator rebuilt for
/// the same module cannot accept a late answer meant for the one it replaced.
static NEXT_FETCH_SEQ: AtomicU64 = AtomicU64::new(0);

/// Chunks requested in one window ahead of the scan cursor when an answer has
/// not shown how dense the peer's map is.
pub(crate) const HINT_WINDOW_CHUNKS: u32 = 1 << 22;

/// Smallest window. A map dense enough to fill every page of a window this
/// size stays unknown past it, which only costs per-offset probing.
const HINT_MIN_WINDOW_CHUNKS: u32 = 1 << 14;

/// A known answer older than this is asked for again, so a body a peer gains
/// later is picked up.
const HINT_REFRESH: Duration = Duration::from_secs(60);

/// Refresh interval for an older peer's full list. Each one is a walk of the
/// peer's whole interval map and up to 64 MiB on the wire.
const HINT_FULL_LIST_REFRESH: Duration = Duration::from_secs(5 * 60);

/// Oldest answer kept in use while its refreshes fail.
const HINT_MAX_STALE: Duration = Duration::from_secs(5 * 60);

/// A complete windowed answer with fewer intervals than this grows the next
/// window. A quarter of [`HINT_MAX_PAGES`] full pages, at the server's
/// 32,768 intervals per page, so a window that only just fit is not grown
/// into one that will be cut.
const HINT_GROW_BELOW_INTERVALS: usize = 32_768 * HINT_MAX_PAGES / 4;

/// Shortest gap between two fetches for one peer when the scan has moved near
/// the edge of its window.
const HINT_MIN_REFETCH: Duration = Duration::from_secs(5);

/// Wait after a failed or unusable answer before asking that peer again.
const HINT_RETRY_AFTER_FAILURE: Duration = Duration::from_secs(60);

/// A fetch with no result after this is dropped, and the previous answer, if
/// any, is used again. Covers all [`HINT_MAX_PAGES`] pages at
/// [`HINT_PAGE_TIMEOUT`] each, plus a minute for the answer to come back.
const HINT_FETCH_TIMEOUT: Duration =
    Duration::from_secs(HINT_PAGE_TIMEOUT.as_secs() * HINT_MAX_PAGES as u64 + 60);

/// Per-page request timeout. An older peer sends its whole list in one page.
pub(crate) const HINT_PAGE_TIMEOUT: Duration = Duration::from_secs(30);

/// Most pages read for one window. The window halves when this is not enough.
pub(crate) const HINT_MAX_PAGES: usize = 16;

/// Offsets this close to the write head are always probed. A peer's map lags
/// chunks that migrated after it answered.
pub(crate) const HINT_FRONTIER_MARGIN: u32 = 16_384;

/// Chunks the head window covers below the write head. Wide enough for the
/// band near the head that every replica may lack while the scan is elsewhere.
pub(crate) const HINT_HEAD_WINDOW_CHUNKS: u32 = 1 << 17;

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
    /// An older peer ignored the window and sent its whole list.
    full_list: bool,
    /// Do not fetch again before this. Set when a refresh failed and this
    /// older answer was kept.
    next_attempt: Option<Instant>,
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
    /// A fetch is in flight. `prior` keeps answering until it lands. Only the
    /// answer to fetch `seq` may replace it.
    Fetching {
        seq: u64,
        started: Instant,
        prior: Option<Known>,
    },
    Known(Known),
}

/// Which of a peer's two windows an answer fills.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowKind {
    /// Follows the scan cursor.
    Scan,
    /// Pinned below the write head. The scan moves on, but the retry set and
    /// a band both replicas lack stay near the head, and dispatch needs an
    /// answer for them too.
    Head,
}

impl PeerHint {
    fn known(&self) -> Option<&Known> {
        match self {
            Self::Known(known)
            | Self::Fetching {
                prior: Some(known), ..
            } => Some(known),
            Self::Unknown { .. } | Self::Fetching { prior: None, .. } => None,
        }
    }

    /// True when this window should be fetched now. A fetch that outlived
    /// [`HINT_FETCH_TIMEOUT`] is dropped first, keeping its previous answer.
    fn due(&mut self, now: Instant, near_edge: impl FnOnce(&Known) -> bool) -> bool {
        match self {
            Self::Unknown { retry_at } => *retry_at <= now,
            Self::Fetching { started, prior, .. } => {
                if now.saturating_duration_since(*started) >= HINT_FETCH_TIMEOUT {
                    *self = match prior.take() {
                        Some(known) => Self::Known(known),
                        None => Self::Unknown {
                            retry_at: now + HINT_RETRY_AFTER_FAILURE,
                        },
                    };
                }
                false
            }
            Self::Known(known) => {
                if known.next_attempt.is_some_and(|next| now < next) {
                    return false;
                }
                let refresh = if known.full_list {
                    HINT_FULL_LIST_REFRESH
                } else {
                    HINT_REFRESH
                };
                let age = now.saturating_duration_since(known.fetched_at);
                age >= refresh || (age >= HINT_MIN_REFETCH && near_edge(known))
            }
        }
    }

    /// Mark this window in flight for fetch `seq`. Its previous answer keeps
    /// applying.
    fn begin_fetch(&mut self, seq: u64, now: Instant) {
        let prior = match std::mem::replace(self, Self::Unknown { retry_at: now }) {
            Self::Known(known) => Some(known),
            Self::Fetching { prior, .. } => prior,
            Self::Unknown { .. } => None,
        };
        *self = Self::Fetching {
            seq,
            started: now,
            prior,
        };
    }
}

#[derive(Debug)]
struct PeerState {
    scan: PeerHint,
    head: PeerHint,
    /// Size of this peer's next scan window. Halves when the pages run out
    /// before the window does.
    window: u32,
}

impl PeerState {
    fn knowns(&self) -> impl Iterator<Item = &Known> {
        self.scan.known().into_iter().chain(self.head.known())
    }
}

/// One planned fetch: read `start..=end` from `peer` into its `kind` window.
/// `seq` ties the answer to this fetch. A late answer to an abandoned fetch
/// for the same window carries an older `seq` and is dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowFetch {
    pub peer: IrysAddress,
    pub start: u32,
    pub end: u32,
    pub kind: WindowKind,
    pub seq: u64,
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

    /// Windows to fetch this tick. Each returned window is marked in flight.
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
        let head_window = (
            max_offset.saturating_sub(HINT_HEAD_WINDOW_CHUNKS - 1),
            max_offset,
        );
        let mut planned = Vec::new();
        let take_seq = || NEXT_FETCH_SEQ.fetch_add(1, Ordering::Relaxed);
        for &peer in assignees {
            let state = self.peers.entry(peer).or_insert(PeerState {
                scan: PeerHint::Unknown { retry_at: now },
                head: PeerHint::Unknown { retry_at: now },
                window: HINT_WINDOW_CHUNKS,
            });
            let window = state.window;
            let (start, end) = window_at(cursor, max_offset, window, reverse);
            if state.scan.due(now, |known| {
                near_window_edge(known, cursor, window, reverse)
            }) {
                let seq = take_seq();
                state.scan.begin_fetch(seq, now);
                planned.push(WindowFetch {
                    peer,
                    start,
                    end,
                    kind: WindowKind::Scan,
                    seq,
                });
            }
            // While the scan window reaches the head it answers for the head
            // band too. So does a full list from an older peer: fetching it
            // again for the head window would download the same list twice.
            let scan_is_full_list = state.scan.known().is_some_and(|known| known.full_list);
            if end < head_window.1 && !scan_is_full_list && state.head.due(now, |_| false) {
                let seq = take_seq();
                state.head.begin_fetch(seq, now);
                planned.push(WindowFetch {
                    peer,
                    start: head_window.0,
                    end: head_window.1,
                    kind: WindowKind::Head,
                    seq,
                });
            }
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
        let hint = match fetch.kind {
            WindowKind::Scan => &mut state.scan,
            WindowKind::Head => &mut state.head,
        };
        // Only the fetch this window is waiting for may fill it. A late answer
        // to a fetch that timed out, or one a newer fetch replaced, is dropped.
        let (started, prior) = match hint {
            PeerHint::Fetching {
                seq,
                started,
                prior,
            } if *seq == fetch.seq => (*started, prior.take()),
            _ => return,
        };
        if fetch.kind == WindowKind::Scan
            && let Ok(page) = &result
            && let IntervalsCoverage::Window { end, .. } = page.coverage
        {
            if end < fetch.end {
                state.window = (state.window / 2).max(HINT_MIN_WINDOW_CHUNKS);
            } else if page.intervals.len() < HINT_GROW_BELOW_INTERVALS {
                state.window = state.window.saturating_mul(2).min(HINT_WINDOW_CHUNKS);
            }
        }
        // Date the answer from when its fetch began: a slow answer is that old.
        let known = result
            .ok()
            .and_then(|page| into_known(page, fetch.start, head, self.frontier_margin, started));
        let hint = match fetch.kind {
            WindowKind::Scan => &mut state.scan,
            WindowKind::Head => &mut state.head,
        };
        // A failed or unusable answer keeps the previous one while it is not
        // too old, and waits before asking again. Dropping it would make the
        // scan probe, and dispatch ask, offsets the peer already answered for.
        let retry_at = now + HINT_RETRY_AFTER_FAILURE;
        *hint = match known {
            Some(known) => PeerHint::Known(known),
            None => match prior
                .filter(|prior| now.saturating_duration_since(prior.fetched_at) < HINT_MAX_STALE)
            {
                Some(mut prior) => {
                    prior.next_attempt = Some(retry_at);
                    PeerHint::Known(prior)
                }
                None => PeerHint::Unknown { retry_at },
            },
        };
    }

    /// Split `start..=end` by what `assignees` claim, ascending. Spans some
    /// assignee has no answer for are [`Coverage::Unknown`].
    ///
    /// The range is cut at every window edge, so each piece is either inside
    /// or outside each window. A piece is answered when every assignee has a
    /// window over all of it.
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
        let mut per_peer: Vec<Vec<&Known>> = Vec::with_capacity(assignees.len());
        let mut cuts = vec![start];
        for peer in assignees {
            let knowns: Vec<&Known> = self
                .peers
                .get(peer)
                .map(|state| state.knowns().collect())
                .unwrap_or_default();
            if knowns.is_empty() {
                return unknown;
            }
            for known in &knowns {
                for edge in [Some(known.covers.0), known.covers.1.checked_add(1)]
                    .into_iter()
                    .flatten()
                {
                    if edge > start && edge <= end {
                        cuts.push(edge);
                    }
                }
            }
            per_peer.push(knowns);
        }
        cuts.sort_unstable();
        cuts.dedup();

        let mut segments: Vec<(u32, u32, Coverage)> = Vec::new();
        for (index, &low) in cuts.iter().enumerate() {
            let high = cuts.get(index + 1).map_or(end, |next| next - 1);
            let mut covering: Vec<&Known> = Vec::new();
            let answered = per_peer.iter().all(|knowns| {
                let before = covering.len();
                covering.extend(
                    knowns
                        .iter()
                        .copied()
                        .filter(|known| known.covers.0 <= low && high <= known.covers.1),
                );
                covering.len() > before
            });
            let pieces = if answered {
                split_by_claims(&covering, low, high)
            } else {
                vec![(low, high, Coverage::Unknown)]
            };
            for piece in pieces {
                match segments.last_mut() {
                    Some(last) if last.2 == piece.2 && last.1 + 1 == piece.0 => last.1 = piece.1,
                    _ => segments.push(piece),
                }
            }
        }
        segments
    }

    /// True when a window of `peer` answers for `offset` and none of them
    /// holds it.
    pub(crate) fn excludes(&self, peer: &IrysAddress, offset: u32) -> bool {
        let Some(state) = self.peers.get(peer) else {
            return false;
        };
        let mut answered = false;
        for known in state.knowns() {
            if known.covers.0 <= offset && offset <= known.covers.1 {
                if known.holds(offset) {
                    return false;
                }
                answered = true;
            }
        }
        answered
    }

    #[cfg(test)]
    pub(crate) fn insert_known(
        &mut self,
        peer: IrysAddress,
        covers: (u32, u32),
        data: Vec<(u32, u32)>,
        now: Instant,
    ) {
        self.insert_known_window(peer, WindowKind::Scan, covers, data, now);
    }

    #[cfg(test)]
    pub(crate) fn insert_known_window(
        &mut self,
        peer: IrysAddress,
        kind: WindowKind,
        covers: (u32, u32),
        data: Vec<(u32, u32)>,
        now: Instant,
    ) {
        let state = self.peers.entry(peer).or_insert(PeerState {
            scan: PeerHint::Unknown { retry_at: now },
            head: PeerHint::Unknown { retry_at: now },
            window: HINT_WINDOW_CHUNKS,
        });
        let known = PeerHint::Known(Known {
            covers,
            data,
            fetched_at: now,
            extendable_low: false,
            extendable_high: false,
            full_list: false,
            next_attempt: None,
        });
        match kind {
            WindowKind::Scan => state.scan = known,
            WindowKind::Head => state.head = known,
        }
    }
}

/// Split `low..=high` into Claimed and Unclaimed spans. Every window in
/// `knowns` covers the whole range; a chunk any of them holds is claimed.
fn split_by_claims(knowns: &[&Known], low: u32, high: u32) -> Vec<(u32, u32, Coverage)> {
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
    segments
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
    let full_list = matches!(page.coverage, IntervalsCoverage::FullList);
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
        full_list,
        next_attempt: None,
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
        assert_eq!(first.len(), 1);
        assert_eq!(
            (first[0].peer, first[0].start, first[0].end, first[0].kind),
            (peer(1), 0, 1_000, WindowKind::Scan),
            "a reverse scan reads the window below the cursor"
        );
        assert!(
            hints
                .plan_fetches(&assignees, 1_000, 1_000, true, now)
                .is_empty(),
            "no second fetch while one is in flight"
        );

        hints.on_fetched(first[0], 1_000, Ok(window(vec![(0, 9)], 1_000, true)), now);
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

    /// A fetch that outlived its timeout is abandoned. Its late answer must
    /// not fill the window a newer fetch is waiting on, and the newer answer
    /// must still land.
    #[test]
    fn a_late_answer_to_an_abandoned_fetch_is_dropped() {
        let now = Instant::now();
        let mut hints = AssigneeIntervals::new(0);
        let stale = hints.plan_fetches(&[peer(1)], 100, 100, true, now)[0];

        let expired = now + HINT_FETCH_TIMEOUT;
        assert!(
            hints
                .plan_fetches(&[peer(1)], 100, 100, true, expired)
                .is_empty()
        );
        let retry = expired + HINT_RETRY_AFTER_FAILURE;
        let fresh = hints.plan_fetches(&[peer(1)], 100, 100, true, retry)[0];
        assert_ne!(stale.seq, fresh.seq);
        assert_eq!(
            (stale.start, stale.end),
            (fresh.start, fresh.end),
            "same window"
        );

        hints.on_fetched(stale, 100, Ok(full(vec![(0, 99)])), retry);
        assert_eq!(
            hints.segments(&[peer(1)], 0, 100),
            vec![(0, 100, Coverage::Unknown)],
            "the abandoned fetch's answer is ignored"
        );
        hints.on_fetched(fresh, 100, Ok(full(vec![(0, 9)])), retry);
        assert_eq!(
            hints.segments(&[peer(1)], 0, 100)[0],
            (0, 9, Coverage::Claimed),
            "the fetch the window waits on still fills it"
        );
    }

    /// A slow answer is as old as its fetch, so it is refreshed on time.
    #[test]
    fn a_slow_answer_is_dated_from_when_its_fetch_began() {
        let now = Instant::now();
        let mut hints = AssigneeIntervals::new(0);
        let fetch = hints.plan_fetches(&[peer(1)], 100, 100, true, now)[0];
        let answered = now + Duration::from_secs(200);
        hints.on_fetched(fetch, 100, Ok(window(vec![(0, 9)], 100, true)), answered);
        assert_eq!(
            hints
                .plan_fetches(&[peer(1)], 100, 100, true, now + HINT_REFRESH)
                .len(),
            1,
            "due one refresh after the fetch began, not after it was answered"
        );
    }

    /// An older peer's full list is refreshed every few minutes, not every
    /// minute.
    #[test]
    fn a_full_list_is_refreshed_less_often() {
        let now = Instant::now();
        let mut hints = AssigneeIntervals::new(0);
        let fetch = hints.plan_fetches(&[peer(1)], 100, 100, true, now)[0];
        hints.on_fetched(fetch, 100, Ok(full(vec![(0, 9)])), now);
        assert!(
            hints
                .plan_fetches(&[peer(1)], 100, 100, true, now + HINT_REFRESH)
                .is_empty()
        );
        assert_eq!(
            hints
                .plan_fetches(&[peer(1)], 100, 100, true, now + HINT_FULL_LIST_REFRESH)
                .len(),
            1
        );
    }

    /// A failed refresh keeps the previous answer, waits before asking again,
    /// and drops the answer once it is too old.
    #[test]
    fn a_failed_refresh_keeps_the_previous_answer_until_it_is_stale() {
        let now = Instant::now();
        let mut hints = AssigneeIntervals::new(0);
        let fetch = hints.plan_fetches(&[peer(1)], 100, 100, true, now)[0];
        hints.on_fetched(fetch, 100, Ok(window(vec![(0, 9)], 100, true)), now);

        let refresh_at = now + HINT_REFRESH;
        let refresh = hints.plan_fetches(&[peer(1)], 100, 100, true, refresh_at)[0];
        hints.on_fetched(refresh, 100, Err(ChunkFetchError::Timeout), refresh_at);
        assert_eq!(
            hints.segments(&[peer(1)], 0, 20)[0],
            (0, 9, Coverage::Claimed),
            "the previous answer still applies"
        );
        assert!(
            hints
                .plan_fetches(&[peer(1)], 100, 100, true, refresh_at + HINT_MIN_REFETCH)
                .is_empty(),
            "a failed refresh backs off"
        );

        let stale_at = now + HINT_MAX_STALE;
        let late = hints.plan_fetches(&[peer(1)], 100, 100, true, stale_at)[0];
        hints.on_fetched(late, 100, Err(ChunkFetchError::Timeout), stale_at);
        assert_eq!(
            hints.segments(&[peer(1)], 0, 20),
            vec![(0, 20, Coverage::Unknown)],
            "an answer this old is dropped"
        );
    }

    /// A window grows only after an answer well inside the page budget.
    #[test]
    fn a_window_grows_only_after_a_light_answer() {
        let now = Instant::now();
        let head = 10_000_000;
        let mut hints = AssigneeIntervals::new(0);
        let fetch = hints.plan_fetches(&[peer(1)], head, head, true, now)[0];
        let mid = fetch.start + HINT_WINDOW_CHUNKS / 2;
        hints.on_fetched(fetch, head, Ok(window(vec![], mid, true)), now);
        let shrunk = HINT_WINDOW_CHUNKS / 2;

        let soon = now + HINT_MIN_REFETCH;
        let fetch = hints.plan_fetches(&[peer(1)], head, head, true, soon)[0];
        assert_eq!(fetch.end - fetch.start + 1, shrunk);
        let heavy: Vec<(u32, u32)> = (0..u32::try_from(HINT_GROW_BELOW_INTERVALS).unwrap())
            .map(|i| (fetch.start + 2 * i, fetch.start + 2 * i))
            .filter(|run| run.1 <= fetch.end)
            .collect();
        hints.on_fetched(fetch, head, Ok(window(heavy, fetch.end, true)), soon);

        let later = soon + HINT_REFRESH;
        let fetch = hints.plan_fetches(&[peer(1)], head, head, true, later)[0];
        assert_eq!(
            fetch.end - fetch.start + 1,
            shrunk,
            "a complete but dense answer does not grow the window"
        );
        hints.on_fetched(fetch, head, Ok(window(vec![], fetch.end, true)), later);
        let fetch = hints.plan_fetches(&[peer(1)], head, head, true, later + HINT_REFRESH)[0];
        assert_eq!(
            fetch.end - fetch.start + 1,
            HINT_WINDOW_CHUNKS,
            "a light one does"
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

    /// Once the scan is far below the head, a second window is fetched at
    /// the head. While the scan window reaches the head, it is not.
    #[test]
    fn a_head_window_is_fetched_only_when_the_scan_has_left_the_head() {
        let now = Instant::now();
        let head = 10_000_000;
        let mut hints = AssigneeIntervals::new(0);
        let at_head = hints.plan_fetches(&[peer(1)], head, head, true, now);
        assert_eq!(at_head.len(), 1, "the scan window covers the head");
        assert_eq!(at_head[0].kind, WindowKind::Scan);

        let mut hints = AssigneeIntervals::new(0);
        let deep = hints.plan_fetches(&[peer(1)], 1_000_000, head, true, now);
        assert_eq!(deep.len(), 2);
        let head_fetch = deep
            .iter()
            .find(|fetch| fetch.kind == WindowKind::Head)
            .expect("a head window");
        assert_eq!(head_fetch.end, head);
        assert_eq!(
            head_fetch.end - head_fetch.start + 1,
            HINT_HEAD_WINDOW_CHUNKS
        );
        hints.on_fetched(*head_fetch, head, Ok(window(vec![], head, true)), now);
        assert!(
            hints
                .plan_fetches(&[peer(1)], 1_000_000, head, true, now)
                .is_empty(),
            "neither window is due again yet"
        );
    }

    /// An older peer sends its whole list for the scan window. That already
    /// covers the head band, so the head window is not fetched again.
    #[test]
    fn a_full_list_from_an_older_peer_is_not_fetched_twice() {
        let now = Instant::now();
        let head = 10_000_000;
        let mut hints = AssigneeIntervals::new(0);
        let first = hints.plan_fetches(&[peer(1)], 1_000_000, head, true, now);
        assert_eq!(first.len(), 2, "nothing is known about the peer yet");
        for fetch in first {
            hints.on_fetched(fetch, head, Ok(full(vec![(0, 9)])), now);
        }

        let later = now + HINT_FULL_LIST_REFRESH;
        let refresh = hints.plan_fetches(&[peer(1)], 1_000_000, head, true, later);
        assert_eq!(
            refresh.iter().map(|fetch| fetch.kind).collect::<Vec<_>>(),
            vec![WindowKind::Scan],
            "the full list answers for the head band"
        );
    }

    /// The head window answers for the band the scan window has moved away
    /// from: dispatch excludes it there and the scan skips it.
    #[test]
    fn the_head_window_answers_where_the_scan_window_does_not() {
        let now = Instant::now();
        let mut hints = AssigneeIntervals::new(0);
        hints.insert_known_window(peer(1), WindowKind::Scan, (0, 99), vec![(0, 9)], now);
        hints.insert_known_window(peer(1), WindowKind::Head, (900, 999), vec![(950, 959)], now);
        assert!(hints.excludes(&peer(1), 905), "answered by the head window");
        assert!(!hints.excludes(&peer(1), 955), "the head window holds it");
        assert!(!hints.excludes(&peer(1), 500), "no window answers here");
        assert_eq!(
            hints.segments(&[peer(1)], 0, 999),
            vec![
                (0, 9, Coverage::Claimed),
                (10, 99, Coverage::Unclaimed),
                (100, 899, Coverage::Unknown),
                (900, 949, Coverage::Unclaimed),
                (950, 959, Coverage::Claimed),
                (960, 999, Coverage::Unclaimed),
            ]
        );
    }

    /// Where a peer's windows overlap, a chunk either one holds is claimed.
    #[test]
    fn overlapping_windows_of_one_peer_favor_a_claim() {
        let now = Instant::now();
        let mut hints = AssigneeIntervals::new(0);
        hints.insert_known_window(peer(1), WindowKind::Scan, (0, 99), vec![], now);
        hints.insert_known_window(peer(1), WindowKind::Head, (50, 149), vec![(60, 69)], now);
        assert!(!hints.excludes(&peer(1), 65));
        assert_eq!(
            hints.segments(&[peer(1)], 0, 149),
            vec![
                (0, 59, Coverage::Unclaimed),
                (60, 69, Coverage::Claimed),
                (70, 149, Coverage::Unclaimed),
            ]
        );
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
                root: irys_types::DataTreeRoot,
                data_size: u64,
                tx_offset: irys_types::TxChunkOffset,
                api: SocketAddr,
                timeout: Duration,
            ) -> Result<irys_types::ChunkFormat, ChunkFetchError> {
                self.0
                    .fetch_chunk_by_data_root(root, data_size, tx_offset, api, timeout)
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
