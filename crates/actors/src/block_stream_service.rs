//! The block-stream producer: the single, node-wide writer of the durable `seq` event log.
//!
//! The producer consumes a lossless, unbounded feed of [`BlockStreamSignal`]s emitted from the
//! node's authoritative sites (the confirmation loop, the reorg site, and the per-block migration
//! loop). For each signal it builds the wire [`StreamEvent`], appends it to the durable log
//! (assigning the next `seq`), and fans it out to live SSE subscribers. Being the sole writer makes
//! `seq` assignment monotonic and gap-free.
//!
//! The HTTP handlers never touch `seq`: they hold an [`Arc<BlockStreamHandle>`] and call the atomic
//! [`BlockStreamHandle::subscribe`], which snapshots the durable replay suffix and registers a live
//! receiver under one lock so the replay→live handover has no gap or duplicate.

use crate::block_tree_service::BlockStreamSignal;
use eyre::OptionExt as _;
use irys_database::db::IrysDatabaseExt as _;
use irys_types::block_stream::{BlockEvent, BlockRef, EventsPage, StreamEvent, StreamFrame};
use irys_types::{DatabaseProvider, H256, TokioServiceHandle};
use lru::LruCache;
use reth::tasks::shutdown::Shutdown;
use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tokio::sync::mpsc::{self, Receiver, Sender, UnboundedReceiver, error::TryRecvError};
use tracing::{Instrument as _, error, info, warn};

/// Count-based retention: keep at most this many events; older ones are pruned. Sized to comfortably
/// exceed the maximum expected follower downtime (the follower only ever resumes from a recent
/// `seq`).
const RETENTION_EVENTS: u64 = 100_000;
/// Prune at most once per this many appends, to batch the delete writes.
const PRUNE_INTERVAL: u64 = 1_000;
/// De-dup window for emitted `observed` block hashes. Must comfortably exceed the reorg depth so a
/// re-adopted block is still remembered.
const DEDUP_CAPACITY: NonZeroUsize = match NonZeroUsize::new(10_000) {
    Some(capacity) => capacity,
    None => NonZeroUsize::MIN,
};
/// Per-subscriber live buffer. A consumer that lags beyond this many frames is dropped (its SSE
/// stream ends) and reconnects with `from_seq` to replay from the durable log — bounding memory
/// instead of letting a stuck follower accumulate frames without limit.
const SUBSCRIBER_BUFFER: usize = 1_024;
/// Maximum number of durable frames decoded at once for an SSE replay.
const REPLAY_PAGE: usize = 1_024;

/// Max frames a single `GET /internal/blocks/events` page may return; an over-size `limit` is clamped to
/// this rather than rejected, bounding per-request work.
const MAX_PAGE: u64 = 1_024;

/// Shared handle: the live fan-out registry plus DB access. Held by the producer task and cloned
/// into `ApiState` so every SSE handler shares the one producer.
#[derive(Debug)]
pub struct BlockStreamHandle {
    /// Live SSE subscribers. The lock is shared with [`Self::append_and_fanout`] so a subscribe's
    /// snapshot+register pair cannot interleave with an append. Bounded senders: a lagging
    /// subscriber is dropped rather than buffered without limit.
    live: Mutex<Vec<Sender<Arc<StreamFrame>>>>,
    db: DatabaseProvider,
}

impl BlockStreamHandle {
    fn new(db: DatabaseProvider) -> Self {
        Self {
            live: Mutex::new(Vec::new()),
            db,
        }
    }

    /// Atomic replay→live handover: under one lock, snapshot the durable replay bounds and register
    /// a live sender, so no append interleaves between the snapshot and the registration. The returned
    /// replay reads that immutable range in bounded pages after releasing the fan-out lock.
    pub fn subscribe(
        &self,
        from_seq: u64,
    ) -> eyre::Result<(ReplayStream, Receiver<Arc<StreamFrame>>)> {
        let mut live = self
            .live
            .lock()
            .map_err(|_| eyre::eyre!("block-stream fan-out lock poisoned"))?;
        let (start, end) = self.db.view_eyre(|tx| {
            let lowest = irys_database::block_stream_lowest_seq(tx)?.unwrap_or(0);
            let logical_len = match irys_database::block_stream_latest_seq(tx)? {
                Some(seq) => seq.checked_add(1).ok_or_eyre("block-stream seq overflow")?,
                None => 0,
            };
            // A cursor beyond the tip is stale — only reachable after a log reset shrank the log. Replay
            // from the retained floor so the follower sees below-cursor frames and rewinds, matching the
            // `/events` beyond-tip clamp; otherwise the SSE follower would silently continue onto the new
            // chain at the same seq. In-window / caught-up cursors replay from `from_seq` (empty at the
            // tip); a below-floor cursor also clamps to the retained floor.
            let start = if from_seq < lowest || from_seq > logical_len {
                lowest
            } else {
                from_seq
            };
            Ok((start, logical_len))
        })?;
        let (tx, rx) = mpsc::channel(SUBSCRIBER_BUFFER);
        live.push(tx);
        Ok((ReplayStream::new(&self.db, start, end), rx))
    }

    /// One-shot page over the durable log for `GET /internal/blocks/events`, read in a single
    /// transaction (first key + last key + bounded range) and decoded with the same [`decode_frame`] the
    /// SSE replay uses, so the frames are byte-identical. Registers no live subscriber and takes no
    /// fan-out lock.
    ///
    /// Three cursor regimes, all valid: an in-window `from_seq` pages from itself (the caught-up
    /// `from_seq == logical_len` is a normal empty page); a `from_seq` below the retained floor returns an
    /// empty `truncated` page whose `next_seq` is the floor (the follower discards frames and resyncs
    /// forward to it); a `from_seq` past the tip clamps to the floor.
    pub fn events_page(&self, from_seq: u64, limit: u64) -> eyre::Result<EventsPage> {
        // checked conversions / arithmetic only — never silently manufacture a wrong cursor
        let limit = usize::try_from(limit.min(MAX_PAGE))?;
        self.db.view_eyre(|tx| {
            let lowest = irys_database::block_stream_lowest_seq(tx)?.unwrap_or(0);
            let logical_len = match irys_database::block_stream_latest_seq(tx)? {
                Some(seq) => seq.checked_add(1).ok_or_eyre("block-stream seq overflow")?,
                None => 0,
            };
            // A below-floor (truncated) page is a pure resync signal: the follower discards any frames
            // and force-resets its cursor forward to `next_seq` (the floor), so carry no frames — with an
            // empty page `next_seq == lowest` is exactly that floor. A beyond-tip cursor clamps to the
            // floor so the follower instead sees a below-cursor frame and rewinds (chain reset, not a
            // gap). In-window pages from `from_seq`, empty when caught up at `from_seq == logical_len`.
            let (start, read_limit, truncated) = if from_seq < lowest {
                (lowest, 0, true) // below the retained floor → signal only, no frames
            } else if from_seq > logical_len {
                (lowest, limit, false) // beyond the tip → clamp to the floor (0 on a fresh log)
            } else {
                (from_seq, limit, false) // in-window / at-tip (== logical_len yields an empty page)
            };
            let raw = irys_database::read_block_stream_range(tx, start, read_limit)?;
            let mut frames = Vec::with_capacity(raw.len());
            for (seq, bytes) in raw {
                frames.push(decode_frame(seq, &bytes)?);
            }
            let count = u64::try_from(frames.len())?;
            let next_seq = start
                .checked_add(count)
                .ok_or_eyre("page cursor overflow")?;
            Ok(EventsPage {
                from_seq,
                next_seq,
                has_more: next_seq < logical_len,
                lowest_retained_seq: lowest,
                truncated,
                frames,
            })
        })
    }

    /// Producer-only. Append `event` to the durable log (assigning `seq`) and fan it out, holding
    /// the same lock as [`Self::subscribe`].
    fn append_and_fanout(&self, event: StreamEvent) -> eyre::Result<u64> {
        let mut live = self
            .live
            .lock()
            .map_err(|_| eyre::eyre!("block-stream fan-out lock poisoned"))?;
        let payload = serde_json::to_vec(&event)?;
        let seq = self
            .db
            .update_eyre(|tx| irys_database::append_block_stream_event(tx, payload))?;
        let frame = Arc::new(StreamFrame { seq, event });
        // Drop subscribers whose receiver is closed or lagging past `SUBSCRIBER_BUFFER`; a dropped
        // follower reconnects and replays from the durable log via `subscribe(from_seq)`.
        live.retain(|sender| {
            sender
                .try_send(Arc::clone(&frame)) // clone: live subscribers share one immutable frame allocation
                .is_ok()
        });
        Ok(seq)
    }

    fn prune(&self, keep_from_seq: u64) -> eyre::Result<()> {
        self.db
            .update_eyre(|tx| irys_database::prune_block_stream_below(tx, keep_from_seq))
    }
}

/// Bounded, lazy replay of the durable range captured by [`BlockStreamHandle::subscribe`].
#[derive(Debug)]
pub struct ReplayStream {
    db: DatabaseProvider,
    next_seq: u64,
    end_seq: u64,
    buffered: VecDeque<Arc<StreamFrame>>,
    failed: bool,
}

impl ReplayStream {
    fn new(db: &DatabaseProvider, next_seq: u64, end_seq: u64) -> Self {
        Self {
            db: DatabaseProvider(Arc::clone(&db.0)), // clone: replay owns shared access to the DB environment
            next_seq,
            end_seq,
            buffered: VecDeque::new(),
            failed: false,
        }
    }

    fn load_page(&mut self) -> eyre::Result<()> {
        let remaining = self.end_seq.saturating_sub(self.next_seq);
        let limit = usize::try_from(remaining)
            .unwrap_or(usize::MAX)
            .min(REPLAY_PAGE);
        let raw = self
            .db
            .view_eyre(|tx| irys_database::read_block_stream_range(tx, self.next_seq, limit))?;
        let mut expected = self.next_seq;
        // Stage the whole page before publishing it: a mid-page decode/seq failure must leave
        // `buffered` untouched so `poll_next` surfaces the error and then terminates, rather than
        // draining already-decoded frames after the error (which would break replay ordering).
        let mut page = VecDeque::new();
        for (seq, bytes) in raw {
            if seq != expected {
                return Err(eyre::eyre!(
                    "block-stream replay lost seq {expected}; next retained seq is {seq}"
                ));
            }
            page.push_back(Arc::new(decode_frame(seq, &bytes)?));
            expected = expected
                .checked_add(1)
                .ok_or_eyre("block-stream replay seq overflow")?;
        }
        if expected == self.next_seq && self.next_seq < self.end_seq {
            return Err(eyre::eyre!(
                "block-stream replay ended at seq {} before snapshot end {}",
                self.next_seq,
                self.end_seq
            ));
        }
        self.buffered = page;
        self.next_seq = expected;
        Ok(())
    }
}

impl futures::Stream for ReplayStream {
    type Item = eyre::Result<Arc<StreamFrame>>;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(frame) = self.buffered.pop_front() {
            return Poll::Ready(Some(Ok(frame)));
        }
        if self.failed || self.next_seq >= self.end_seq {
            return Poll::Ready(None);
        }
        if let Err(error) = self.load_page() {
            self.failed = true;
            return Poll::Ready(Some(Err(error)));
        }
        Poll::Ready(self.buffered.pop_front().map(Ok))
    }
}

/// Rebuilds a [`StreamFrame`] from a stored `(seq, event-json)` log entry.
fn decode_frame(seq: u64, bytes: &[u8]) -> eyre::Result<StreamFrame> {
    let event: StreamEvent = serde_json::from_slice(bytes)?;
    Ok(StreamFrame { seq, event })
}

/// Spawns the block-stream producer task and returns its service handle plus the shared
/// [`BlockStreamHandle`] for `ApiState`.
pub struct BlockStreamService;

impl BlockStreamService {
    pub fn spawn_service(
        signal_rx: UnboundedReceiver<BlockStreamSignal>,
        db: DatabaseProvider,
        chunk_size: u64,
        runtime_handle: tokio::runtime::Handle,
    ) -> (TokioServiceHandle, Arc<BlockStreamHandle>) {
        info!("Spawning block-stream service");
        let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();
        let handle = Arc::new(BlockStreamHandle::new(db));
        let producer_handle = Arc::clone(&handle); // clone: producer and API share the service handle

        let join = runtime_handle.spawn(
            async move {
                Producer::new(producer_handle, chunk_size, shutdown_rx, signal_rx)
                    .run()
                    .await;
            }
            .in_current_span(),
        );

        let service_handle = TokioServiceHandle {
            name: "block_stream_service".to_string(),
            handle: join,
            shutdown_signal: shutdown_tx,
        };
        (service_handle, handle)
    }
}

struct Producer {
    handle: Arc<BlockStreamHandle>,
    chunk_size: u64,
    shutdown: Shutdown,
    signal_rx: UnboundedReceiver<BlockStreamSignal>,
    /// `block_hash`es already emitted as `observed` (or carried in a reorg's `new_fork`), so a
    /// re-confirmed block is not re-emitted.
    emitted: LruCache<H256, ()>,
    /// `block_hash`es already emitted as `finalized`, to guard against a double `finalized` for the
    /// same block. Keyed by hash (not height) so a block re-migrated at a previously-finalised
    /// height after deep-reorg recovery (`recover_from_network_partition`) still emits.
    finalized: LruCache<H256, ()>,
    appends_since_prune: u64,
}

impl Producer {
    fn new(
        handle: Arc<BlockStreamHandle>,
        chunk_size: u64,
        shutdown: Shutdown,
        signal_rx: UnboundedReceiver<BlockStreamSignal>,
    ) -> Self {
        let (emitted, finalized) = rebuild_state(&handle.db);
        Self {
            handle,
            chunk_size,
            shutdown,
            signal_rx,
            emitted,
            finalized,
            appends_since_prune: 0,
        }
    }

    async fn run(&mut self) {
        info!("block-stream producer started");
        loop {
            tokio::select! {
                _ = &mut self.shutdown => {
                    if let Err(e) = self.drain_queued_signals() {
                        error!(error = ?e, "block-stream producer halting while draining shutdown queue");
                    } else {
                        info!("block-stream producer drained queued signals and is shutting down");
                    }
                    break;
                }
                maybe_signal = self.signal_rx.recv() => {
                    match maybe_signal {
                        Some(signal) => {
                            if let Err(e) = self.handle_signal(signal) {
                                // A durable append failed: halt rather than silently skip the event
                                // and keep assigning later `seq`s, which would break the lossless-log
                                // contract. Followers reconnect and replay up to the last good `seq`.
                                error!(error = ?e, "block-stream producer halting: durable append failed");
                                break;
                            }
                        }
                        None => {
                            info!("block-stream signal channel closed; producer stopping");
                            break;
                        }
                    }
                }
            }
        }
    }

    fn drain_queued_signals(&mut self) -> eyre::Result<()> {
        self.signal_rx.close();
        loop {
            match self.signal_rx.try_recv() {
                Ok(signal) => self.handle_signal(signal)?,
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return Ok(()),
            }
        }
    }

    fn handle_signal(&mut self, signal: BlockStreamSignal) -> eyre::Result<()> {
        match signal {
            BlockStreamSignal::Confirmed(block) => {
                let hash = block.header().block_hash;
                if self.emitted.contains(&hash) {
                    return Ok(());
                }
                let event = StreamEvent::Observed(BlockEvent::from_sealed(&block, self.chunk_size));
                self.append(event)?;
                self.emitted.put(hash, ());
            }
            BlockStreamSignal::Finalized(block) => {
                let hash = block.header().block_hash;
                if self.finalized.contains(&hash) {
                    return Ok(());
                }
                let event =
                    StreamEvent::Finalized(BlockEvent::from_sealed(&block, self.chunk_size));
                self.append(event)?;
                self.finalized.put(hash, ());
            }
            BlockStreamSignal::Reorged {
                fork_parent,
                old_fork,
                new_fork,
            } => {
                let event = StreamEvent::Reorged {
                    fork_parent: BlockRef {
                        height: fork_parent.height,
                        block_hash: fork_parent.block_hash,
                    },
                    orphaned: old_fork
                        .iter()
                        .map(|b| BlockEvent::from_sealed(b, self.chunk_size))
                        .collect(),
                    new_fork: new_fork
                        .iter()
                        .map(|b| BlockEvent::from_sealed(b, self.chunk_size))
                        .collect(),
                };
                self.append(event)?;
                // Mark the new-fork hashes seen so the `Confirmed` signals that follow this tick do
                // not also emit `observed` for them: `propagate_block` sends the `Reorged` signal
                // before the per-block `Confirmed` signals, and the signal channel is FIFO.
                for block in new_fork.iter() {
                    self.emitted.put(block.header().block_hash, ());
                }
            }
        }
        Ok(())
    }

    fn append(&mut self, event: StreamEvent) -> eyre::Result<()> {
        let seq = self.handle.append_and_fanout(event)?;
        self.appends_since_prune += 1;
        if self.appends_since_prune >= PRUNE_INTERVAL {
            self.appends_since_prune = 0;
            if seq + 1 > RETENTION_EVENTS {
                let keep_from = seq + 1 - RETENTION_EVENTS;
                if let Err(e) = self.handle.prune(keep_from) {
                    warn!(error = ?e, "block-stream log prune failed");
                }
            }
        }
        Ok(())
    }
}

/// Rebuilds the producer's in-memory de-dup state (`observed` and `finalized` hashes) from the
/// durable log tail on startup, so a restart does not re-emit for blocks already in the log.
fn rebuild_state(db: &DatabaseProvider) -> (LruCache<H256, ()>, LruCache<H256, ()>) {
    let mut emitted = LruCache::new(DEDUP_CAPACITY);
    let mut finalized = LruCache::new(DEDUP_CAPACITY);

    let tail = (|| -> eyre::Result<Vec<(u64, Vec<u8>)>> {
        let Some(latest) = db.view_eyre(irys_database::block_stream_latest_seq)? else {
            return Ok(Vec::new());
        };
        let capacity = u64::try_from(DEDUP_CAPACITY.get()).unwrap_or(u64::MAX);
        let start = latest.saturating_sub(capacity);
        db.view_eyre(|tx| irys_database::read_block_stream_from(tx, start))
    })();

    match tail {
        Ok(events) => {
            for (_seq, bytes) in &events {
                match serde_json::from_slice::<StreamEvent>(bytes) {
                    Ok(StreamEvent::Observed(block)) => {
                        emitted.put(block.header.block_hash, ());
                    }
                    Ok(StreamEvent::Finalized(block)) => {
                        finalized.put(block.header.block_hash, ());
                    }
                    Ok(StreamEvent::Reorged { new_fork, .. }) => {
                        for block in new_fork {
                            emitted.put(block.header.block_hash, ());
                        }
                    }
                    Err(e) => {
                        warn!(error = ?e, "skipping undecodable block-stream log entry during rebuild");
                    }
                }
            }
        }
        Err(e) => {
            warn!(error = ?e, "block-stream state rebuild failed; starting with empty de-dup state");
        }
    }

    (emitted, finalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{StreamExt as _, TryStreamExt as _};
    use irys_database::{
        IrysDatabaseArgs as _, append_block_stream_event, open_or_create_db,
        prune_block_stream_below, tables::IrysTables,
    };
    use irys_types::block_stream::{BlockEvent, BlockHeaderView, OwnerId};
    use irys_types::{BlockTransactions, IrysBlockHeader, SealedBlock};
    use reth_db::mdbx::DatabaseArguments;

    /// A minimal but well-formed `observed` event, so `decode_frame` round-trips it. The frame's `seq`
    /// comes from the DB key, not this body, so a constant body is fine for the regime assertions.
    fn sample_stream_event() -> StreamEvent {
        StreamEvent::Observed(BlockEvent {
            header: BlockHeaderView {
                height: 0,
                block_hash: H256::zero(),
                previous_block_hash: H256::zero(),
                timestamp: 0,
                miner_address: OwnerId {
                    sig_type: 0,
                    bytes: vec![0_u8; 20],
                },
                data_ledgers: vec![],
            },
            txs: vec![],
        })
    }

    fn sample_event() -> Vec<u8> {
        serde_json::to_vec(&sample_stream_event()).expect("serialize sample event")
    }

    fn collect_replay(replay: ReplayStream) -> Vec<Arc<StreamFrame>> {
        futures::executor::block_on(replay.try_collect()).expect("collect replay")
    }

    fn sample_block(height: u64) -> Arc<SealedBlock> {
        let mut header = IrysBlockHeader::default();
        header.height = height;
        header.block_hash = H256::from_low_u64_be(height);
        Arc::new(SealedBlock::new_unchecked(
            Arc::new(header),
            BlockTransactions::default(),
        ))
    }

    fn handle_with_events(
        n: u64,
    ) -> (
        BlockStreamHandle,
        irys_testing_utils::utils::tempfile::TempDir,
    ) {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db_env = open_or_create_db(
            tmp.path(),
            IrysTables::ALL,
            DatabaseArguments::irys_testing().unwrap(),
        )
        .unwrap();
        let db = DatabaseProvider(Arc::new(db_env));
        for _ in 0..n {
            db.update_eyre(|tx| append_block_stream_event(tx, sample_event()))
                .unwrap();
        }
        (BlockStreamHandle::new(db), tmp)
    }

    #[test]
    fn events_page_regimes() {
        let (handle, _tmp) = handle_with_events(3); // seqs 0,1,2 → logical_len = 3

        // in-window: contiguous suffix from from_seq
        let page = handle.events_page(1, 10).unwrap();
        assert_eq!(
            page.frames.iter().map(|f| f.seq).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(page.from_seq, 1);
        assert_eq!(page.next_seq, 3);
        assert!(!page.has_more);
        assert!(!page.truncated);
        assert_eq!(page.lowest_retained_seq, 0);

        // limit honoured
        let page = handle.events_page(0, 1).unwrap();
        assert_eq!(page.frames.len(), 1);
        assert_eq!(page.next_seq, 1);
        assert!(page.has_more);

        // caught-up (== logical_len): empty page, not a clamp
        let page = handle.events_page(3, 10).unwrap();
        assert!(page.frames.is_empty());
        assert_eq!(page.next_seq, 3);
        assert!(!page.has_more);
        assert!(!page.truncated);

        // beyond tip (> logical_len): clamp to floor 0
        let page = handle.events_page(8, 10).unwrap();
        assert_eq!(page.frames.first().map(|f| f.seq), Some(0));
        assert!(!page.truncated);

        // limit == 0 probe: empty frames, correct envelope
        let page = handle.events_page(1, 0).unwrap();
        assert!(page.frames.is_empty());
        assert_eq!(page.next_seq, 1);
        assert!(page.has_more);
    }

    #[test]
    fn events_page_empty_log() {
        let (handle, _tmp) = handle_with_events(0);
        let page = handle.events_page(0, 10).unwrap();
        assert!(page.frames.is_empty());
        assert_eq!(page.next_seq, 0);
        assert!(!page.has_more);
        assert!(!page.truncated);
        assert_eq!(page.lowest_retained_seq, 0);
    }

    #[test]
    fn events_page_below_floor_truncates() {
        let (handle, _tmp) = handle_with_events(5); // seqs 0..=4 → logical_len = 5
        // Drive pruning directly (RETENTION_EVENTS is a non-configurable const): floor → 3.
        handle
            .db
            .update_eyre(|tx| prune_block_stream_below(tx, 3))
            .unwrap();

        let page = handle.events_page(0, 1).unwrap();
        assert!(page.truncated);
        // A truncated page is a resync signal: no frames, and next_seq is the floor the follower
        // force-resets forward to (it discards frames and resumes from next_seq).
        assert!(page.frames.is_empty());
        assert_eq!(page.lowest_retained_seq, 3);
        assert_eq!(page.next_seq, 3);
        assert!(page.has_more); // floor (3) < logical_len (5)
    }

    #[test]
    fn events_page_frames_match_sse_replay() {
        let (handle, _tmp) = handle_with_events(3);
        let page = handle.events_page(0, 10).unwrap();
        let (replay, _live) = handle.subscribe(0).unwrap();
        let replay = collect_replay(replay);
        assert_eq!(
            serde_json::to_value(&page.frames).unwrap(),
            serde_json::to_value(&replay).unwrap(),
            "poll frames must be byte-identical to the SSE replay"
        );
    }

    #[test]
    fn subscribe_clamps_stale_cursor_to_floor() {
        let (handle, _tmp) = handle_with_events(3); // seqs 0,1,2 → logical_len = 3
        // A cursor beyond the tip (only reachable after a reset shrank the log) replays from the floor,
        // so the follower sees below-cursor frames and rewinds — not an empty replay.
        let (replay, _live) = handle.subscribe(99).unwrap();
        let replay = collect_replay(replay);
        assert_eq!(replay.first().map(|f| f.seq), Some(0));
        assert_eq!(replay.len(), 3);
        // Caught up at the tip replays nothing — no re-stream of the whole log.
        let (replay, _live) = handle.subscribe(3).unwrap();
        let replay = collect_replay(replay);
        assert!(replay.is_empty());
    }

    #[test]
    fn subscribe_replay_decodes_at_most_one_page_at_a_time() {
        let event_count = u64::try_from(REPLAY_PAGE).unwrap() + 1;
        let (handle, _tmp) = handle_with_events(event_count);
        let (mut replay, _live) = handle.subscribe(0).unwrap();

        assert!(replay.buffered.is_empty());
        let first = futures::executor::block_on(replay.next())
            .expect("first replay item")
            .expect("decode first replay item");

        assert_eq!(first.seq, 0);
        assert_eq!(replay.buffered.len(), REPLAY_PAGE - 1);
    }

    #[test]
    fn live_subscribers_share_the_same_frame_allocation() {
        let (handle, _tmp) = handle_with_events(0);
        let (_replay_a, mut live_a) = handle.subscribe(0).unwrap();
        let (_replay_b, mut live_b) = handle.subscribe(0).unwrap();

        handle.append_and_fanout(sample_stream_event()).unwrap();
        let frame_a = live_a.blocking_recv().expect("first live frame");
        let frame_b = live_b.blocking_recv().expect("second live frame");

        assert!(Arc::ptr_eq(&frame_a, &frame_b));
    }

    #[test]
    fn subscribe_reports_a_poisoned_fanout_lock() {
        let (handle, _tmp) = handle_with_events(0);
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = handle.live.lock().expect("lock fan-out registry");
            panic!("poison fan-out registry");
        }));
        assert!(poisoned.is_err());

        let error = handle
            .subscribe(0)
            .expect_err("poisoned lock must be an error");
        assert!(error.to_string().contains("fan-out lock poisoned"));
    }

    #[test]
    fn shutdown_drain_persists_every_queued_signal() {
        let (handle, _tmp) = handle_with_events(0);
        let handle = Arc::new(handle);
        let (_shutdown_tx, shutdown) = reth::tasks::shutdown::signal();
        let (signal_tx, signal_rx) = mpsc::unbounded_channel();
        signal_tx
            .send(BlockStreamSignal::Confirmed(sample_block(1)))
            .expect("queue first signal");
        signal_tx
            .send(BlockStreamSignal::Confirmed(sample_block(2)))
            .expect("queue second signal");
        let mut producer = Producer::new(Arc::clone(&handle), 1, shutdown, signal_rx);

        producer.drain_queued_signals().expect("drain signals");

        let (replay, _live) = handle.subscribe(0).unwrap();
        let frames = collect_replay(replay);
        assert_eq!(
            frames.iter().map(|frame| frame.seq).collect::<Vec<_>>(),
            vec![0, 1]
        );
    }
}
