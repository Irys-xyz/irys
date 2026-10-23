//! Receiver-side state for chunk announcements: which pushes are inside
//! `handle_chunk` now, and which claims a fetch task owns. Whether this node
//! holds a chunk is a `CachedChunks` lookup done by the caller; this module
//! never asserts possession. The gossip seen-cache records what peers know
//! and is not used to decide whether to fetch.

use crate::wire_types::ChunkAnnouncement;
use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use irys_types::{ChunkPathHash, H256, IrysPeerId};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::Semaphore;

/// Caps and timings. `Default` is the production policy from the design.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkAnnounceLimits {
    pub max_claims: usize,
    pub max_per_peer: usize,
    pub max_sources: usize,
    /// Later announcers kept per claim; one takes the slot of a failed source.
    pub max_backlog: usize,
    pub max_concurrent_fetches: usize,
    pub max_concurrent_serves: usize,
    /// Whole-task budget, from the moment the claim is admitted.
    pub fetch_deadline: Duration,
    /// Waits before rounds 2 and 3. Round 1 waits `chunk_fetch_delay_ms`.
    pub later_round_delays: [Duration; 2],
    /// One ingest attempt. A timeout is not a cancellation.
    pub ingest_timeout: Duration,
    /// Sleeps between ingest retries on local `Overloaded`.
    pub overload_backoff: [Duration; 3],
    /// Poll interval while a push for the hash is arriving.
    pub arriving_poll: Duration,
}

impl Default for ChunkAnnounceLimits {
    fn default() -> Self {
        Self {
            max_claims: 10_000,
            max_per_peer: 1_000,
            max_sources: 4,
            max_backlog: 12,
            max_concurrent_fetches: 32,
            max_concurrent_serves: 16,
            // Same budget data sync gives one delivered body (DELIVERY_TIMEOUT).
            fetch_deadline: Duration::from_secs(60),
            later_round_delays: [Duration::from_secs(2), Duration::from_secs(8)],
            ingest_timeout: Duration::from_secs(10),
            overload_backoff: [
                Duration::from_millis(250),
                Duration::from_secs(1),
                Duration::from_secs(4),
            ],
            arriving_poll: Duration::from_millis(100),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkGossipStatsSnapshot {
    pub bodies_push: u64,
    pub bodies_fetch: u64,
    pub announcements_received: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AnnounceDecision {
    /// The claim already has a fetch task; the announcer was kept as a
    /// source or in the backlog if there was room and it had budget.
    Joined,
    /// A new claim was created; the caller starts exactly one fetch task.
    StartFetch,
    /// A cap is reached. Answer RateLimited; nothing was allocated.
    OverCapacity,
}

/// Map key for a claim. The offset is stored as `u32` so the key can be
/// hashed without depending on `TxChunkOffset`'s manual `Hash` impl.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ClaimKey {
    data_root: H256,
    data_size: u64,
    tx_offset: u32,
    chunk_path_hash: ChunkPathHash,
}

impl From<&ChunkAnnouncement> for ClaimKey {
    fn from(announcement: &ChunkAnnouncement) -> Self {
        Self {
            data_root: announcement.data_root,
            data_size: announcement.data_size,
            tx_offset: announcement.tx_offset.0,
            chunk_path_hash: announcement.chunk_path_hash,
        }
    }
}

#[derive(Debug, Default)]
struct Claim {
    /// Asked in order each round; at most `max_sources`.
    sources: Vec<IrysPeerId>,
    /// Later announcers, at most `max_backlog`; the front one takes the slot
    /// of a source that fails.
    backlog: VecDeque<IrysPeerId>,
}

#[derive(Debug)]
pub struct ChunkAnnounceState {
    limits: ChunkAnnounceLimits,
    arriving: DashMap<ChunkPathHash, usize>,
    claims: DashMap<ClaimKey, Claim>,
    claim_count: AtomicUsize,
    per_peer: DashMap<IrysPeerId, usize>,
    bodies_push: AtomicU64,
    bodies_fetch: AtomicU64,
    announcements_received: AtomicU64,
    pub(crate) fetch_permits: Arc<Semaphore>,
    pub(crate) serve_permits: Arc<Semaphore>,
}

impl Default for ChunkAnnounceState {
    fn default() -> Self {
        Self::new()
    }
}

impl ChunkAnnounceState {
    pub fn new() -> Self {
        Self::with_limits(ChunkAnnounceLimits::default())
    }

    pub fn with_limits(limits: ChunkAnnounceLimits) -> Self {
        Self {
            limits,
            arriving: DashMap::new(),
            claims: DashMap::new(),
            claim_count: AtomicUsize::new(0),
            per_peer: DashMap::new(),
            bodies_push: AtomicU64::new(0),
            bodies_fetch: AtomicU64::new(0),
            announcements_received: AtomicU64::new(0),
            fetch_permits: Arc::new(Semaphore::new(limits.max_concurrent_fetches)),
            serve_permits: Arc::new(Semaphore::new(limits.max_concurrent_serves)),
        }
    }

    pub(crate) fn limits(&self) -> ChunkAnnounceLimits {
        self.limits
    }

    /// Decide what an announcement does. The caller has already checked
    /// that the chunk is not held.
    pub(crate) fn admit(
        &self,
        announcement: &ChunkAnnouncement,
        announcer: IrysPeerId,
    ) -> AnnounceDecision {
        // Lock order: a `claims` shard, then a `per_peer` shard. `finish`
        // releases the `claims` entry before touching `per_peer`, so the
        // reverse order never occurs.
        match self.claims.entry(ClaimKey::from(announcement)) {
            Entry::Occupied(mut entry) => {
                let claim = entry.get_mut();
                let known =
                    claim.sources.contains(&announcer) || claim.backlog.contains(&announcer);
                if !known && claim.sources.len() < self.limits.max_sources {
                    if self.try_reserve_peer(announcer) {
                        claim.sources.push(announcer);
                    }
                } else if !known
                    && claim.backlog.len() < self.limits.max_backlog
                    && self.try_reserve_peer(announcer)
                {
                    claim.backlog.push_back(announcer);
                }
                AnnounceDecision::Joined
            }
            Entry::Vacant(entry) => {
                if !self.try_reserve_claim() {
                    return AnnounceDecision::OverCapacity;
                }
                if !self.try_reserve_peer(announcer) {
                    self.claim_count.fetch_sub(1, Ordering::AcqRel);
                    return AnnounceDecision::OverCapacity;
                }
                entry.insert(Claim {
                    sources: vec![announcer],
                    backlog: VecDeque::new(),
                });
                AnnounceDecision::StartFetch
            }
        }
    }

    fn try_reserve_claim(&self) -> bool {
        self.claim_count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < self.limits.max_claims).then_some(count + 1)
            })
            .is_ok()
    }

    fn try_reserve_peer(&self, peer: IrysPeerId) -> bool {
        let mut count = self.per_peer.entry(peer).or_insert(0);
        if *count >= self.limits.max_per_peer {
            return false;
        }
        *count += 1;
        true
    }

    fn release_peer(&self, peer: &IrysPeerId) {
        if let Entry::Occupied(mut entry) = self.per_peer.entry(*peer) {
            let count = entry.get_mut();
            *count = count.saturating_sub(1);
            if *count == 0 {
                entry.remove();
            }
        }
    }

    /// The claim's active sources, in slot order. Empty once the claim is gone.
    #[cfg(test)]
    pub(crate) fn sources(&self, announcement: &ChunkAnnouncement) -> Vec<IrysPeerId> {
        self.claims
            .get(&ClaimKey::from(announcement))
            // clone: the DashMap guard cannot leave this function
            .map(|claim| claim.sources.clone())
            .unwrap_or_default()
    }

    /// The claim's backlog, front first.
    #[cfg(test)]
    pub(crate) fn backlog(&self, announcement: &ChunkAnnouncement) -> Vec<IrysPeerId> {
        self.claims
            .get(&ClaimKey::from(announcement))
            .map(|claim| claim.backlog.iter().copied().collect())
            .unwrap_or_default()
    }

    /// The first active source not in `tried`. A source promoted from the
    /// backlog during a round is not in `tried`, so the same round asks it.
    pub(crate) fn next_source(
        &self,
        announcement: &ChunkAnnouncement,
        tried: &[IrysPeerId],
    ) -> Option<IrysPeerId> {
        self.claims
            .get(&ClaimKey::from(announcement))?
            .sources
            .iter()
            .find(|source| !tried.contains(source))
            .copied()
    }

    /// `source` had no usable body. The front backlog announcer takes its
    /// slot and `source`'s reservation is released. With an empty backlog
    /// `source` stays, because its failure may be transient.
    pub(crate) fn source_failed(&self, announcement: &ChunkAnnouncement, source: IrysPeerId) {
        let released = {
            let Some(mut claim) = self.claims.get_mut(&ClaimKey::from(announcement)) else {
                return;
            };
            let Some(position) = claim.sources.iter().position(|slot| *slot == source) else {
                return;
            };
            let Some(next) = claim.backlog.pop_front() else {
                return;
            };
            if let Some(slot) = claim.sources.get_mut(position) {
                *slot = next;
            }
            source
        };
        // The claims guard is dropped above; per_peer is locked only after.
        self.release_peer(&released);
    }

    /// Remove a claim and release every reservation it holds.
    pub(crate) fn finish(&self, announcement: &ChunkAnnouncement) {
        let removed = self.claims.remove(&ClaimKey::from(announcement));
        if let Some((_, claim)) = removed {
            self.claim_count.fetch_sub(1, Ordering::AcqRel);
            for peer in claim.sources.iter().chain(claim.backlog.iter()) {
                self.release_peer(peer);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn claim_count(&self) -> usize {
        self.claim_count.load(Ordering::Acquire)
    }

    pub(crate) fn is_arriving(&self, hash: &ChunkPathHash) -> bool {
        self.arriving.contains_key(hash)
    }

    /// Count a push as inside `handle_chunk` until the guard drops.
    pub(crate) fn begin_arriving(&self, hash: ChunkPathHash) -> ArrivingGuard<'_> {
        *self.arriving.entry(hash).or_insert(0) += 1;
        ArrivingGuard { state: self, hash }
    }

    pub(crate) fn record_push_body(&self) {
        self.bodies_push.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_fetch_body(&self) {
        self.bodies_fetch.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_announcement(&self) {
        self.announcements_received.fetch_add(1, Ordering::Relaxed);
    }

    /// Per-node counters. Tests read these; the OTel counters are shared by
    /// every node in one process.
    pub fn stats(&self) -> ChunkGossipStatsSnapshot {
        ChunkGossipStatsSnapshot {
            bodies_push: self.bodies_push.load(Ordering::Relaxed),
            bodies_fetch: self.bodies_fetch.load(Ordering::Relaxed),
            announcements_received: self.announcements_received.load(Ordering::Relaxed),
        }
    }
}

pub(crate) struct ArrivingGuard<'a> {
    state: &'a ChunkAnnounceState,
    hash: ChunkPathHash,
}

impl Drop for ArrivingGuard<'_> {
    fn drop(&mut self) {
        if let Entry::Occupied(mut entry) = self.state.arriving.entry(self.hash) {
            let count = entry.get_mut();
            *count = count.saturating_sub(1);
            if *count == 0 {
                entry.remove();
            }
        }
    }
}

/// Owned by a fetch task. Releases the claim on every exit, including
/// panic and cancellation by the task's deadline.
pub(crate) struct ClaimGuard {
    state: Arc<ChunkAnnounceState>,
    announcement: ChunkAnnouncement,
}

impl ClaimGuard {
    pub(crate) fn new(state: Arc<ChunkAnnounceState>, announcement: ChunkAnnouncement) -> Self {
        Self {
            state,
            announcement,
        }
    }
}

impl Drop for ClaimGuard {
    fn drop(&mut self) {
        self.state.finish(&self.announcement);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::{IrysAddress, TxChunkOffset};

    fn claim(hash: H256, tx_offset: u32) -> ChunkAnnouncement {
        ChunkAnnouncement {
            data_root: H256::repeat_byte(1),
            data_size: 64,
            tx_offset: TxChunkOffset::from(tx_offset),
            chunk_path_hash: hash,
        }
    }

    fn peer(byte: u8) -> IrysPeerId {
        IrysPeerId::from(IrysAddress::repeat_byte(byte))
    }

    fn limits(max_claims: usize, max_per_peer: usize, max_sources: usize) -> ChunkAnnounceLimits {
        ChunkAnnounceLimits {
            max_claims,
            max_per_peer,
            max_sources,
            ..ChunkAnnounceLimits::default()
        }
    }

    #[test]
    fn first_announcement_starts_fetch_and_later_ones_join() {
        let state = ChunkAnnounceState::with_limits(limits(10, 10, 2));
        let announcement = claim(H256::random(), 0);
        assert_eq!(
            state.admit(&announcement, peer(1)),
            AnnounceDecision::StartFetch
        );
        assert_eq!(
            state.admit(&announcement, peer(2)),
            AnnounceDecision::Joined
        );
        assert_eq!(
            state.admit(&announcement, peer(3)),
            AnnounceDecision::Joined
        );
        assert_eq!(
            state.admit(&announcement, peer(1)),
            AnnounceDecision::Joined
        );
        assert_eq!(
            state.sources(&announcement),
            vec![peer(1), peer(2)],
            "deduplicated, capped at 2"
        );
        assert_eq!(
            state.backlog(&announcement),
            vec![peer(3)],
            "the third announcer waits in the backlog"
        );
    }

    #[test]
    fn backlog_is_capped() {
        let state = ChunkAnnounceState::with_limits(ChunkAnnounceLimits {
            max_backlog: 1,
            ..limits(10, 10, 2)
        });
        let announcement = claim(H256::random(), 0);
        for byte in 1..=4 {
            state.admit(&announcement, peer(byte));
        }
        assert_eq!(state.sources(&announcement), vec![peer(1), peer(2)]);
        assert_eq!(
            state.backlog(&announcement),
            vec![peer(3)],
            "peer 4 is accepted but not kept"
        );
    }

    #[test]
    fn a_failed_source_is_replaced_from_the_backlog() {
        let state = ChunkAnnounceState::with_limits(ChunkAnnounceLimits {
            max_backlog: 2,
            ..limits(10, 1, 1)
        });
        let announcement = claim(H256::random(), 0);
        assert_eq!(
            state.admit(&announcement, peer(1)),
            AnnounceDecision::StartFetch
        );
        assert_eq!(
            state.admit(&announcement, peer(2)),
            AnnounceDecision::Joined
        );
        assert_eq!(state.next_source(&announcement, &[]), Some(peer(1)));
        state.source_failed(&announcement, peer(1));
        assert_eq!(state.sources(&announcement), vec![peer(2)]);
        assert!(state.backlog(&announcement).is_empty());
        assert_eq!(
            state.next_source(&announcement, &[peer(1)]),
            Some(peer(2)),
            "promoted source is tried in the same round"
        );
        // max_per_peer is 1, so this succeeds only if peer 1's reservation was released.
        assert_eq!(
            state.admit(&claim(H256::random(), 0), peer(1)),
            AnnounceDecision::StartFetch
        );
    }

    #[test]
    fn a_failed_source_stays_when_the_backlog_is_empty() {
        let state = ChunkAnnounceState::with_limits(limits(10, 10, 2));
        let announcement = claim(H256::random(), 0);
        state.admit(&announcement, peer(1));
        state.source_failed(&announcement, peer(1));
        assert_eq!(
            state.sources(&announcement),
            vec![peer(1)],
            "kept for the next round"
        );
        assert_eq!(
            state.next_source(&announcement, &[peer(1)]),
            None,
            "already tried this round"
        );
    }

    #[test]
    fn finish_releases_backlog_reservations() {
        let state = ChunkAnnounceState::with_limits(limits(10, 1, 1));
        let announcement = claim(H256::random(), 0);
        state.admit(&announcement, peer(1));
        state.admit(&announcement, peer(2));
        state.finish(&announcement);
        assert_eq!(
            state.admit(&claim(H256::random(), 0), peer(2)),
            AnnounceDecision::StartFetch
        );
    }

    #[test]
    fn conflicting_identity_for_one_hash_is_a_separate_claim() {
        let state = ChunkAnnounceState::with_limits(limits(10, 10, 4));
        let hash = H256::random();
        let wrong = claim(hash, 7);
        let right = claim(hash, 0);
        assert_eq!(state.admit(&wrong, peer(1)), AnnounceDecision::StartFetch);
        assert_eq!(state.admit(&right, peer(2)), AnnounceDecision::StartFetch);
        assert_eq!(state.sources(&wrong), vec![peer(1)]);
        assert_eq!(state.sources(&right), vec![peer(2)]);
        assert_eq!(state.claim_count(), 2);
    }

    #[test]
    fn caps_reject_without_allocating() {
        let state = ChunkAnnounceState::with_limits(limits(1, 10, 4));
        assert_eq!(
            state.admit(&claim(H256::random(), 0), peer(1)),
            AnnounceDecision::StartFetch
        );
        let over = claim(H256::random(), 0);
        assert_eq!(state.admit(&over, peer(2)), AnnounceDecision::OverCapacity);
        assert_eq!(state.claim_count(), 1);
        assert!(state.sources(&over).is_empty());

        let state = ChunkAnnounceState::with_limits(limits(10, 1, 4));
        assert_eq!(
            state.admit(&claim(H256::random(), 0), peer(1)),
            AnnounceDecision::StartFetch
        );
        assert_eq!(
            state.admit(&claim(H256::random(), 0), peer(1)),
            AnnounceDecision::OverCapacity
        );
        assert_eq!(
            state.admit(&claim(H256::random(), 0), peer(2)),
            AnnounceDecision::StartFetch
        );
        assert_eq!(state.claim_count(), 2);
    }

    #[test]
    fn finish_releases_the_claim_and_its_reservations() {
        let state = ChunkAnnounceState::with_limits(limits(1, 1, 4));
        let announcement = claim(H256::random(), 0);
        assert_eq!(
            state.admit(&announcement, peer(1)),
            AnnounceDecision::StartFetch
        );
        state.finish(&announcement);
        assert_eq!(state.claim_count(), 0);
        assert!(state.sources(&announcement).is_empty());
        assert_eq!(
            state.admit(&claim(H256::random(), 0), peer(1)),
            AnnounceDecision::StartFetch
        );
    }

    #[test]
    fn claim_guard_finishes_on_drop() {
        let state = std::sync::Arc::new(ChunkAnnounceState::with_limits(limits(1, 1, 4)));
        let announcement = claim(H256::random(), 0);
        assert_eq!(
            state.admit(&announcement, peer(1)),
            AnnounceDecision::StartFetch
        );
        drop(ClaimGuard::new(std::sync::Arc::clone(&state), announcement));
        assert_eq!(state.claim_count(), 0);
    }

    #[test]
    fn arriving_is_reference_counted() {
        let state = ChunkAnnounceState::new();
        let hash = H256::random();
        let first = state.begin_arriving(hash);
        let second = state.begin_arriving(hash);
        drop(first);
        assert!(
            state.is_arriving(&hash),
            "one push still inside handle_chunk"
        );
        drop(second);
        assert!(!state.is_arriving(&hash));
    }

    #[test]
    fn stats_snapshot_counts() {
        let state = ChunkAnnounceState::new();
        state.record_push_body();
        state.record_fetch_body();
        state.record_fetch_body();
        state.record_announcement();
        assert_eq!(
            state.stats(),
            ChunkGossipStatsSnapshot {
                bodies_push: 1,
                bodies_fetch: 2,
                announcements_received: 1
            }
        );
    }
}
