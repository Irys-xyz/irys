//! The block-stream producer: live fan-out, retention, and startup reconciliation for the durable
//! `seq` event log.
//!
//! The log has multiple writers, serialised by the consensus environment's single RW transaction:
//! confirmation appends `observed`/`reorged` frames and migration appends `finalized` frames
//! inside their own canonical transactions (`BlockMigrationService::persist_metadata` /
//! `persist_block`), which makes each frame atomic with the state transition it reports. The
//! producer receives each already-durable frame over an unbounded channel and only fans it out to
//! live SSE subscribers — it opens no RW transaction on that path. The producer itself appends
//! solely during [`Producer::reconcile_finalized_tail`], which repairs `finalized` frames a
//! pre-atomic-append build lost to a crash between its migration commit and its producer append.
//!
//! Because production frames commit with their transitions, a crash cannot lose an `observed`,
//! `reorged`, or `finalized` frame for a transition that persisted; the residual loss window is
//! confined to the reconciliation appends themselves. Restart re-emission is suppressed at the
//! writer: `BlockMigrationService` seeds its de-dup from the log tail ([`rebuild_state`]).
//!
//! The HTTP handlers never touch `seq`: they hold an [`Arc<BlockStreamHandle>`] and call
//! [`BlockStreamHandle::subscribe`], which snapshots the durable replay suffix and registers a
//! live receiver under one lock; frames whose `seq` predates a subscriber's snapshot are skipped
//! at fan-out, so the replay→live handover has no gap or duplicate.

use eyre::OptionExt as _;
use irys_database::db::IrysDatabaseExt as _;
use irys_types::block_stream::{BlockEvent, EventsPage, StreamEvent, StreamFrame};
use irys_types::{DatabaseProvider, H256, TokioServiceHandle};
use lru::LruCache;
use reth::tasks::shutdown::Shutdown;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
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
pub(crate) const DEDUP_CAPACITY: NonZeroUsize = match NonZeroUsize::new(10_000) {
    Some(capacity) => capacity,
    None => NonZeroUsize::MIN,
};
/// Per-subscriber live buffer. A consumer that lags beyond this many frames is dropped (its SSE
/// stream ends) and reconnects with `from_seq` to replay from the durable log — bounding memory
/// instead of letting a stuck follower accumulate frames without limit.
const SUBSCRIBER_BUFFER: usize = 1_024;

/// Max frames a single `GET /internal/blocks/events` page may return; an over-size `limit` is clamped to
/// this rather than rejected, bounding per-request work.
const MAX_PAGE: u64 = 1_024;

/// Upper bound on the backward block-index scan in [`Producer::reconcile_finalized_tail`]. A real
/// crash gap is at most the migration batch that was in flight; a scan this deep means the log and
/// index tails do not meet at all, and reconciliation aborts instead of replaying history.
const RECONCILE_SCAN_CAP: u64 = 1_000;

/// Shared handle: the live fan-out registry plus DB access. Held by the producer task and cloned
/// into `ApiState` so every SSE handler shares the one producer.
#[derive(Debug)]
pub struct BlockStreamHandle {
    /// Live SSE subscribers. The lock serialises registration against fan-out (and against
    /// reconciliation's [`Self::append_and_fanout`]); production appends commit outside it and
    /// rely on per-subscriber `replay_end` to suppress replay/live duplicates. Bounded senders: a
    /// lagging subscriber is dropped rather than buffered without limit.
    live: Mutex<LiveSubscribers>,
    db: DatabaseProvider,
}

/// One live subscriber: bounded sender plus the durable-log `end` snapped at registration.
/// Fan-out skips frames with `seq < replay_end` so a frame committed before the snapshot cannot
/// be delivered both in the replay range and on the live channel.
#[derive(Debug)]
struct LiveSubscriber {
    sender: Sender<Arc<StreamFrame>>,
    /// Exclusive upper bound of the durable replay snapshot (`[start, end)`).
    replay_end: u64,
}

/// The live fan-out registry: subscriber senders plus a `closed` flag the producer sets when it
/// stops appending for good. Once closed, [`BlockStreamHandle::subscribe`] registers no new sender,
/// so a reconnecting follower's SSE ends cleanly after its replay instead of hanging on a live tail
/// nothing will ever feed.
#[derive(Debug)]
struct LiveSubscribers {
    senders: Vec<LiveSubscriber>,
    closed: bool,
}

impl BlockStreamHandle {
    fn new(db: DatabaseProvider) -> Self {
        Self {
            live: Mutex::new(LiveSubscribers {
                senders: Vec::new(),
                closed: false,
            }),
            db,
        }
    }

    /// Handle for nodes that do not run the durable block-stream producer
    /// (`http.expose_internal_api = false`). Live subscribe registers no sender;
    /// `/internal/*` routes that use this handle are unmounted when the flag is off.
    pub fn disabled(db: DatabaseProvider) -> Self {
        Self {
            live: Mutex::new(LiveSubscribers {
                senders: Vec::new(),
                closed: true,
            }),
            db,
        }
    }

    /// Replay→live handover: snapshot the durable replay bounds `[start, end)` and register a live
    /// sender under one lock. The lock serialises registration against fan-out only — durable
    /// appends commit outside it — so a frame committed around the snapshot can reach both the
    /// replay range and the live channel; [`Self::fanout_locked`] suppresses the live copy for any
    /// `seq < replay_end`. The caller replays `[start, end)` via [`Self::replay_page`] (after the
    /// lock is released), then tails the live receiver.
    pub fn subscribe(&self, from_seq: u64) -> eyre::Result<(u64, u64, Receiver<Arc<StreamFrame>>)> {
        let mut live = self
            .live
            .lock()
            .map_err(|_| eyre::eyre!("block-stream fan-out lock poisoned"))?;
        let (start, end) = self.db.view_eyre(|tx| {
            let (lowest, logical_len) = irys_database::block_stream_log_bounds(tx)?;
            // Below-floor and beyond-tip (post-reset) cursors replay from the retained floor — the
            // SSE rewind; the poll endpoint instead signals `truncated` for a below-floor cursor.
            let start = if from_seq < lowest || from_seq > logical_len {
                lowest
            } else {
                from_seq
            };
            Ok((start, logical_len))
        })?;
        let (tx, rx) = mpsc::channel(SUBSCRIBER_BUFFER);
        // After the producer has halted for good, register no new live sender: dropping `tx` here
        // closes `rx`, so the reconnecting follower's SSE ends cleanly after its replay rather than
        // hanging on a live tail nothing will ever feed.
        if !live.closed {
            live.senders.push(LiveSubscriber {
                sender: tx,
                replay_end: end,
            });
        }
        Ok((start, end, rx))
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
        let limit = usize::try_from(limit.min(MAX_PAGE))?;
        self.db.view_eyre(|tx| {
            let (lowest, logical_len) = irys_database::block_stream_log_bounds(tx)?;
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

    /// One bounded page of an SSE subscriber's durable replay, capped by the immutable snapshot `end`
    /// that [`Self::subscribe`] captured. Returns `(next_cursor, frames)` for a contiguous page from
    /// `cursor`, or `None` when the replay is complete (`cursor >= end`) or must abort because a prune
    /// advanced the retained floor past `cursor` mid-replay (a `truncated`/no-progress page). On abort the
    /// cursor has not reached `end`, which is how the caller tells "resync" from "done".
    ///
    /// Bounding each page by `end` (not [`Self::events_page`]'s `has_more`, which tracks a moving
    /// tip) keeps the replay→live handover gap- and duplicate-free.
    pub fn replay_page(
        &self,
        cursor: u64,
        end: u64,
    ) -> eyre::Result<Option<(u64, Vec<StreamFrame>)>> {
        if cursor >= end {
            return Ok(None);
        }
        let page = self.events_page(cursor, end - cursor)?;
        if page.truncated || page.next_seq <= cursor {
            return Ok(None);
        }
        Ok(Some((page.next_seq, page.frames)))
    }

    /// Append `event` to the durable log (assigning `seq`) and fan it out, holding the same lock
    /// as [`Self::subscribe`]. Startup reconciliation is the only production caller; the hot path
    /// appends inside confirmation/migration txns and calls [`Self::fanout_only`].
    fn append_and_fanout(&self, event: StreamEvent) -> eyre::Result<u64> {
        let payload = serde_json::to_vec(&event)?;
        // The lock must cover the durable append, not just the fan-out: `subscribe` snapshots its
        // replay bound and registers its live sender under this lock, so an append interleaving
        // between those two would be both replayed and pushed live to the new subscriber.
        let mut live = self
            .live
            .lock()
            .map_err(|_| eyre::eyre!("block-stream fan-out lock poisoned"))?;
        let seq = self
            .db
            .update_eyre(|tx| irys_database::append_block_stream_event(tx, payload))?;
        let frame = Arc::new(StreamFrame { seq, event });
        Self::fanout_locked(&mut live, &frame);
        Ok(seq)
    }

    /// Fan out a frame that is already durable (the production path). Does not open a RW txn.
    /// Subscribers whose `replay_end` already covers `frame.seq` are skipped (no live/replay dup).
    fn fanout_only(&self, frame: &Arc<StreamFrame>) -> eyre::Result<()> {
        let mut live = self
            .live
            .lock()
            .map_err(|_| eyre::eyre!("block-stream fan-out lock poisoned"))?;
        Self::fanout_locked(&mut live, frame);
        Ok(())
    }

    fn fanout_locked(live: &mut LiveSubscribers, frame: &Arc<StreamFrame>) {
        // Drop subscribers whose receiver is closed or lagging past `SUBSCRIBER_BUFFER`; a dropped
        // follower reconnects and replays from the durable log via `subscribe(from_seq)`.
        // Skip delivery when `frame.seq < replay_end` — that seq is already in the subscriber's
        // durable replay window (fan-out can trail a late subscribe that snapped past it).
        live.senders.retain(|sub| {
            if frame.seq < sub.replay_end {
                return true; // keep subscriber; do not deliver a replay-covered frame
            }
            sub.sender
                .try_send(Arc::clone(frame)) // clone: live subscribers share one immutable frame allocation
                .is_ok()
        });
    }

    fn prune(&self, keep_from_seq: u64) -> eyre::Result<()> {
        self.db
            .update_eyre(|tx| irys_database::prune_block_stream_below(tx, keep_from_seq))
    }

    /// Drops every live subscriber sender so their SSE streams end and followers reconnect to
    /// replay from the last durable `seq`. Called when the producer stops appending for good.
    fn close_live_subscribers(&self) {
        match self.live.lock() {
            Ok(mut live) => {
                live.closed = true;
                live.senders.clear();
            }
            Err(_) => warn!("block-stream fan-out lock poisoned while closing subscribers"),
        }
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
        signal_rx: UnboundedReceiver<Arc<StreamFrame>>,
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
    /// Already-durable frames from the confirmation/migration writers, to fan out live.
    signal_rx: UnboundedReceiver<Arc<StreamFrame>>,
    appends_since_prune: u64,
}

impl Producer {
    fn new(
        handle: Arc<BlockStreamHandle>,
        chunk_size: u64,
        shutdown: Shutdown,
        signal_rx: UnboundedReceiver<Arc<StreamFrame>>,
    ) -> Self {
        Self {
            handle,
            chunk_size,
            shutdown,
            signal_rx,
            appends_since_prune: 0,
        }
    }

    async fn run(&mut self) {
        info!("block-stream producer started");
        // Fan out frames already queued before reconciling, so their (lower) seqs reach the wire
        // before anything reconciliation appends. Frames committed after this drain are covered
        // by reconciliation's single-txn snapshot (never re-appended); only their live fan-out
        // can trail reconciliation's, and a follower's durable replay corrects that on reconnect.
        if let Err(e) = self.drain_pending_frames() {
            error!(error = ?e, "block-stream producer halting: pre-reconcile fan-out failed");
            crate::metrics::record_block_stream_halted();
            self.handle.close_live_subscribers();
            return;
        }
        if let Err(e) = self.reconcile_finalized_tail() {
            error!(error = ?e, "block-stream producer halting: finalized reconciliation failed");
            crate::metrics::record_block_stream_halted();
            self.handle.close_live_subscribers();
            return;
        }
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
                maybe_frame = self.signal_rx.recv() => {
                    match maybe_frame {
                        Some(frame) => {
                            if let Err(e) = self.handle_frame(frame) {
                                // Fan-out or prune failed (poisoned lock / DB fault). Halt and
                                // disconnect subscribers; the log itself stays durable and
                                // followers replay it on reconnect.
                                error!(error = ?e, "block-stream producer halting: fan-out failed");
                                crate::metrics::record_block_stream_halted();
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
        // The producer has stopped and will append no further frames; disconnect live subscribers
        // rather than leave their SSE streams hanging, so followers reconnect and replay from the
        // last durable `seq`.
        self.handle.close_live_subscribers();
    }

    /// Shutdown path: stop accepting new frames, then fan out whatever is queued.
    fn drain_queued_signals(&mut self) -> eyre::Result<()> {
        self.signal_rx.close();
        self.drain_pending_frames()
    }

    /// Fans out every frame currently queued without closing the channel.
    fn drain_pending_frames(&mut self) -> eyre::Result<()> {
        loop {
            match self.signal_rx.try_recv() {
                Ok(frame) => self.handle_frame(frame)?,
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return Ok(()),
            }
        }
    }

    /// Re-derives `finalized` frames a pre-atomic-append build lost to a crash between a block's
    /// migration commit and its producer append. Walks the block index backward from its tip
    /// until it meets a hash the log already finalised, then appends the gap in ascending height
    /// order. Migration now appends its frame inside the migration txn, so this repairs only logs
    /// written by older builds (or the producer-append paths themselves).
    ///
    /// The log tail and the index are snapshotted in ONE read transaction: a migration committing
    /// concurrently lands in both or neither, so an already-logged block can never look missing
    /// (the duplicate-`finalized` race a stale pre-read de-dup cache would allow).
    ///
    /// Runs only when the log tail holds at least one finalised hash: on a young or freshly-reset
    /// log the index tail predates the log entirely, and "reconciling" it would emit `finalized`
    /// for deep history a follower bootstraps from the canonical reads instead. The same reasoning
    /// caps the backward scan — a real gap is at most the migration batch that was in flight, so a
    /// scan [`RECONCILE_SCAN_CAP`] deep means the log and index tails do not meet, and
    /// reconciliation aborts rather than replay history.
    fn reconcile_finalized_tail(&mut self) -> eyre::Result<()> {
        let missing = self.handle.db.view_eyre(|tx| {
            let mut missing: Vec<(u64, H256)> = Vec::new();
            let (_, finalized) = rebuild_state_in(tx)?;
            if finalized.is_empty() {
                return Ok(missing);
            }
            let Some(latest) = irys_database::block_index_latest_height(tx)? else {
                return Ok(missing);
            };
            for height in (0..=latest).rev() {
                let Some(hash) = irys_database::block_index_hash_by_height(tx, height)? else {
                    break;
                };
                if finalized.contains(&hash) {
                    break;
                }
                missing.push((height, hash));
                if missing.len() as u64 >= RECONCILE_SCAN_CAP {
                    warn!(
                        scanned = missing.len(),
                        "block-stream finalized reconciliation hit its scan cap; skipping \
                         (the log and index tails do not meet)"
                    );
                    missing.clear();
                    break;
                }
            }
            Ok(missing)
        })?;

        for (height, hash) in missing.into_iter().rev() {
            let event = self.handle.db.view_eyre(|tx| {
                let header =
                    irys_database::block_header_by_hash(tx, &hash, false)?.ok_or_else(|| {
                        eyre::eyre!("migrated block {hash} at height {height} has no header row")
                    })?;
                let mut resolve_err: Option<eyre::Report> = None;
                let event = BlockEvent::from_header_and_txs(
                    &header,
                    |ledger| match irys_database::block_ledger_tx_headers(tx, &header, ledger) {
                        Ok(txs) => txs,
                        Err(e) => {
                            resolve_err.get_or_insert(e);
                            Vec::new()
                        }
                    },
                    self.chunk_size,
                );
                match resolve_err {
                    Some(e) => Err(e),
                    None => Ok(event),
                }
            })?;
            info!(
                block.height = height,
                block.hash = %hash,
                "appending finalized frame reconciled from the block index"
            );
            self.append(StreamEvent::Finalized(event))?;
        }
        Ok(())
    }

    /// Production path: the frame is already durable; fan it out and maybe prune.
    fn handle_frame(&mut self, frame: Arc<StreamFrame>) -> eyre::Result<()> {
        self.handle.fanout_only(&frame)?;
        self.maybe_prune(frame.seq)
    }

    fn append(&mut self, event: StreamEvent) -> eyre::Result<()> {
        let seq = self.handle.append_and_fanout(event)?;
        self.maybe_prune(seq)
    }

    fn maybe_prune(&mut self, seq: u64) -> eyre::Result<()> {
        self.appends_since_prune += 1;
        if self.appends_since_prune >= PRUNE_INTERVAL {
            self.appends_since_prune = 0;
            if let Some(keep_from) = seq
                .checked_add(1)
                .and_then(|len| len.checked_sub(RETENTION_EVENTS))
                && let Err(e) = self.handle.prune(keep_from)
            {
                warn!(error = ?e, "block-stream log prune failed");
            }
        }
        Ok(())
    }
}

/// Rebuilds the stream de-dup state (`observed` and `finalized` hashes) from the durable log
/// tail, so a restart does not re-emit for blocks already in the log. Seeds
/// `BlockMigrationService`'s writer-side de-dup at construction.
///
/// An individual entry that fails to decode is skipped with a warning: unlike the serving path
/// (which must error rather than put a `seq` gap on the wire), the rebuild only mines hashes out
/// of the tail.
pub(crate) fn rebuild_state(
    db: &DatabaseProvider,
) -> eyre::Result<(LruCache<H256, ()>, LruCache<H256, ()>)> {
    db.view_eyre(rebuild_state_in)
}

/// [`rebuild_state`] against an already-open read transaction, so callers (reconciliation) can
/// snapshot the log tail and other tables atomically.
fn rebuild_state_in<T: reth_db::transaction::DbTx>(
    tx: &T,
) -> eyre::Result<(LruCache<H256, ()>, LruCache<H256, ()>)> {
    let mut emitted = LruCache::new(DEDUP_CAPACITY);
    let mut finalized = LruCache::new(DEDUP_CAPACITY);

    let events = match irys_database::block_stream_latest_seq(tx)? {
        None => Vec::new(),
        Some(latest) => {
            let capacity = u64::try_from(DEDUP_CAPACITY.get()).unwrap_or(u64::MAX);
            irys_database::read_block_stream_from(tx, latest.saturating_sub(capacity))?
        }
    };

    for (_seq, bytes) in &events {
        match serde_json::from_slice::<StreamEvent>(bytes) {
            Ok(StreamEvent::Observed(block)) => {
                emitted.put(block.header.block_hash, ());
            }
            Ok(StreamEvent::Finalized(block)) => {
                finalized.put(block.header.block_hash, ());
            }
            Ok(StreamEvent::Reorged {
                orphaned, new_fork, ..
            }) => {
                for block in new_fork {
                    emitted.put(block.header.block_hash, ());
                }
                // Mirror the live `Reorged` handling: a rolled-back block must be free to emit
                // `finalized` again if it re-migrates after re-adoption.
                for block in orphaned {
                    finalized.pop(&block.header.block_hash);
                }
            }
            Err(e) => {
                warn!(error = ?e, "skipping undecodable block-stream log entry during rebuild");
            }
        }
    }

    Ok((emitted, finalized))
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn collect_replay(handle: &BlockStreamHandle, start: u64, end: u64) -> Vec<StreamFrame> {
        let mut cursor = start;
        let mut frames = Vec::new();
        while let Some((next, page)) = handle.replay_page(cursor, end).expect("replay_page") {
            frames.extend(page);
            cursor = next;
        }
        frames
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
        let (start, end, _live) = handle.subscribe(0).unwrap();
        let replay = collect_replay(&handle, start, end);
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
        let (start, end, _live) = handle.subscribe(99).unwrap();
        assert_eq!((start, end), (0, 3)); // clamped to the retained floor, not the stale cursor
        let replay = collect_replay(&handle, start, end);
        assert_eq!(replay.first().map(|f| f.seq), Some(0));
        assert_eq!(replay.len(), 3);
        // Caught up at the tip replays nothing — no re-stream of the whole log.
        let (start, end, _live) = handle.subscribe(3).unwrap();
        assert_eq!((start, end), (3, 3));
        assert!(collect_replay(&handle, start, end).is_empty());
    }

    #[test]
    fn subscribe_below_floor_replays_from_floor_while_poll_truncates() {
        let (handle, _tmp) = handle_with_events(5); // seqs 0..=4 → logical_len = 5
        handle
            .db
            .update_eyre(|tx| prune_block_stream_below(tx, 3))
            .unwrap(); // floor → 3

        // SSE side: subscribe clamps the below-floor cursor to the floor and replays frames-from-floor.
        let (start, end, _live) = handle.subscribe(0).unwrap();
        assert_eq!((start, end), (3, 5));
        assert_eq!(
            collect_replay(&handle, start, end)
                .iter()
                .map(|f| f.seq)
                .collect::<Vec<_>>(),
            vec![3, 4]
        );

        // Poll side: the raw below-floor cursor gets an empty, truncated resync page — the
        // deliberate SSE-rewinds/poll-signals asymmetry.
        let page = handle.events_page(0, 10).unwrap();
        assert!(page.truncated && page.frames.is_empty());
        assert_eq!(page.next_seq, 3);
    }

    #[test]
    fn replay_page_bounds_by_end_not_logical_len() {
        let (handle, _tmp) = handle_with_events(3); // logical_len = 3
        let (start, end, _live) = handle.subscribe(0).unwrap(); // end captured = 3
        // The log grows after subscribe; the replay must still stop at the captured `end`, never reading
        // seq 3 or 4 (which belong to the live tail).
        handle
            .db
            .update_eyre(|tx| append_block_stream_event(tx, sample_event()))
            .unwrap();
        handle
            .db
            .update_eyre(|tx| append_block_stream_event(tx, sample_event()))
            .unwrap();
        assert_eq!(
            collect_replay(&handle, start, end)
                .iter()
                .map(|f| f.seq)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        // replay_page reports done exactly at `end`, not at the new tip.
        assert!(handle.replay_page(end, end).unwrap().is_none());
    }

    #[test]
    fn replay_page_aborts_below_advanced_floor() {
        let (handle, _tmp) = handle_with_events(5); // seqs 0..=4
        handle
            .db
            .update_eyre(|tx| prune_block_stream_below(tx, 3))
            .unwrap(); // floor → 3

        // A subscriber that had progressed to cursor 1 is now below the advanced floor: replay_page
        // aborts (None) with the cursor still short of `end` — how the handler tells resync from done.
        assert!(handle.replay_page(1, 5).unwrap().is_none());
        // Resuming from the new floor pages normally.
        let (next, frames) = handle.replay_page(3, 5).unwrap().expect("page from floor");
        assert_eq!(next, 5);
        assert_eq!(frames.iter().map(|f| f.seq).collect::<Vec<_>>(), vec![3, 4]);
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
    fn subscribe_after_close_returns_a_closed_live_receiver() {
        let (handle, _tmp) = handle_with_events(2);
        handle.close_live_subscribers();

        // Replay still delivers the durable suffix up to the last good seq...
        let (start, end, mut live) = handle.subscribe(0).unwrap();
        assert_eq!(collect_replay(&handle, start, end).len(), 2);
        // ...but no live sender is registered, so the receiver is already closed and the follower's
        // SSE ends after replay instead of hanging on a tail the halted producer will never feed.
        assert!(live.blocking_recv().is_none());
    }

    #[test]
    fn shutdown_drain_fans_out_every_queued_frame() {
        let (handle, _tmp) = handle_with_events(0);
        let handle = Arc::new(handle);
        let (_start, _end, mut live) = handle.subscribe(0).unwrap(); // replay_end = 0
        let (_shutdown_tx, shutdown) = reth::tasks::shutdown::signal();
        let (signal_tx, signal_rx) = mpsc::unbounded_channel();

        // Frames are durable before they reach the channel (the writer appends in its own txn).
        for seq in 0..2_u64 {
            let event = sample_stream_event();
            handle
                .db
                .update_eyre(|tx| append_block_stream_event(tx, serde_json::to_vec(&event)?))
                .unwrap();
            signal_tx
                .send(Arc::new(StreamFrame { seq, event }))
                .expect("queue frame");
        }
        let mut producer = Producer::new(Arc::clone(&handle), 1, shutdown, signal_rx);

        producer.drain_queued_signals().expect("drain frames");

        let mut delivered = Vec::new();
        while let Ok(frame) = live.try_recv() {
            delivered.push(frame.seq);
        }
        assert_eq!(
            delivered,
            vec![0, 1],
            "drain must fan out every queued frame"
        );
    }

    /// Covers `fanout_locked`'s `seq < replay_end` skip: a late subscribe must not
    /// receive live copies of frames already inside its durable replay window.
    #[test]
    fn fanout_skips_frames_covered_by_subscriber_replay() {
        let (handle, _tmp) = handle_with_events(2); // durable seqs 0,1 → logical_len = 2
        let (_start, end, mut live) = handle.subscribe(0).unwrap();
        assert_eq!(end, 2);

        let covered = Arc::new(StreamFrame {
            seq: 1,
            event: sample_stream_event(),
        });
        handle.fanout_only(&covered).unwrap();
        assert!(
            live.try_recv().is_err(),
            "replay-covered frame must not be delivered live"
        );

        let fresh = Arc::new(StreamFrame {
            seq: 2,
            event: sample_stream_event(),
        });
        handle.fanout_only(&fresh).unwrap();
        assert_eq!(live.try_recv().expect("live frame").seq, 2);
    }

    /// A producer these tests drive directly, so the returned shutdown/sender halves are unused
    /// and may drop.
    fn producer_over(handle: &Arc<BlockStreamHandle>) -> Producer {
        let (_shutdown_tx, shutdown) = reth::tasks::shutdown::signal();
        let (_signal_tx, signal_rx) = mpsc::unbounded_channel();
        Producer::new(Arc::clone(handle), 1, shutdown, signal_rx)
    }

    fn finalized_event(block: &Arc<SealedBlock>) -> StreamEvent {
        StreamEvent::Finalized(BlockEvent::from_sealed(block, 1))
    }

    fn reorged_event(orphaned: &Arc<SealedBlock>, new_fork: &Arc<SealedBlock>) -> StreamEvent {
        let fork_parent = IrysBlockHeader::default();
        StreamEvent::Reorged {
            fork_parent: irys_types::block_stream::BlockRef {
                height: fork_parent.height,
                block_hash: fork_parent.block_hash,
            },
            orphaned: vec![BlockEvent::from_sealed(orphaned, 1)],
            new_fork: vec![BlockEvent::from_sealed(new_fork, 1)],
        }
    }

    /// The CX-1 regression, now at the rebuild layer that seeds the writer-side de-dup: a block
    /// finalised, then orphaned by a reorg, must be free to emit `finalized` again after its fork
    /// is re-adopted — so the rebuilt state must mirror the reorg eviction.
    #[test]
    fn rebuild_state_evicts_orphaned_finalized_for_readoption() {
        let (handle, _tmp) = handle_with_events(0);
        let handle = Arc::new(handle);

        let block_b = sample_block(1);
        let block_c = sample_block(2);

        handle
            .append_and_fanout(finalized_event(&block_b))
            .expect("finalize B");
        handle
            .append_and_fanout(reorged_event(&block_b, &block_c))
            .expect("reorg orphaning B");

        // Restart here — between the reorg and the re-migration: the rebuilt de-dup state must
        // mirror the live eviction, so the re-finalise below survives either path.
        let (emitted, finalized) = rebuild_state(&handle.db).expect("rebuild");
        assert!(
            !finalized.contains(&block_b.header().block_hash),
            "rebuild must evict the orphaned hash from the finalized de-dup"
        );
        assert!(emitted.contains(&block_c.header().block_hash));

        handle
            .append_and_fanout(finalized_event(&block_b))
            .expect("re-finalize B after re-adoption");

        let (start, end, _live) = handle.subscribe(0).unwrap();
        let kinds: Vec<&str> = collect_replay(&handle, start, end)
            .iter()
            .map(StreamFrame::kind)
            .collect();
        assert_eq!(kinds, vec!["finalized", "reorged", "finalized"]);

        // After the re-finalise is durable, a rebuild suppresses a further duplicate again.
        let (_, finalized) = rebuild_state(&handle.db).expect("rebuild after re-finalise");
        assert!(finalized.contains(&block_b.header().block_hash));
    }

    /// The CR-7 reconciliation: a `finalized` frame lost to a crash between the migration commit
    /// and the append is re-derived from the block index at startup and appended in height order.
    #[test]
    fn startup_reconciles_finalized_frames_missing_from_the_log_tail() {
        use irys_types::BlockIndexItem;

        let (handle, _tmp) = handle_with_events(0);
        let handle = Arc::new(handle);

        // Block 1 migrated AND logged; blocks 2 and 3 migrated (index + headers committed) but
        // their finalized frames were lost by a pre-atomic-append build's crash window.
        let blocks: Vec<Arc<SealedBlock>> = (1_u64..=3).map(sample_block).collect();
        handle
            .append_and_fanout(finalized_event(&blocks[0]))
            .expect("finalize block 1");
        handle
            .db
            .update_eyre(|tx| {
                for block in &blocks {
                    let header = block.header();
                    irys_database::insert_block_header(tx, header)?;
                    irys_database::insert_block_index_item(
                        tx,
                        header.height,
                        &BlockIndexItem {
                            block_hash: header.block_hash,
                            ..Default::default()
                        },
                    )?;
                }
                Ok(())
            })
            .expect("seed index and headers");

        // A fresh producer (as after a restart) reconciles the gap before serving.
        let mut producer = producer_over(&handle);
        producer
            .reconcile_finalized_tail()
            .expect("reconciliation succeeds");

        let (start, end, _live) = handle.subscribe(0).unwrap();
        let frames = collect_replay(&handle, start, end);
        let finalized_hashes: Vec<H256> = frames
            .iter()
            .filter(|f| f.kind() == "finalized")
            .filter_map(StreamFrame::block_hash)
            .collect();
        assert_eq!(
            finalized_hashes,
            vec![
                blocks[0].header().block_hash,
                blocks[1].header().block_hash,
                blocks[2].header().block_hash,
            ],
            "the missing tail is appended once, in ascending height order"
        );

        // Idempotent: a second reconciliation (say, another restart) appends nothing.
        let mut producer = producer_over(&handle);
        producer
            .reconcile_finalized_tail()
            .expect("second reconciliation");
        let (_, end_after, _live) = handle.subscribe(0).unwrap();
        assert_eq!(end, end_after, "reconciliation is idempotent");
    }

    /// The duplicate-`finalized` race: a migration that commits its index row and frame AFTER the
    /// producer starts (but before reconciliation runs) must not be re-appended. Reconciliation
    /// snapshots the log tail and the index in one read txn, so the committed frame is visible.
    #[test]
    fn reconcile_skips_frames_committed_after_producer_creation() {
        use irys_types::BlockIndexItem;

        let (handle, _tmp) = handle_with_events(0);
        let handle = Arc::new(handle);

        // Log tail non-empty at producer creation (block 1 finalised and logged).
        let block_1 = sample_block(1);
        handle
            .append_and_fanout(finalized_event(&block_1))
            .expect("finalize block 1");
        let mut producer = producer_over(&handle);

        // Migration of block 2 commits — index row AND finalized frame — after the producer
        // exists but before reconciliation runs.
        let block_2 = sample_block(2);
        handle
            .db
            .update_eyre(|tx| {
                for block in [&block_1, &block_2] {
                    let header = block.header();
                    irys_database::insert_block_header(tx, header)?;
                    irys_database::insert_block_index_item(
                        tx,
                        header.height,
                        &BlockIndexItem {
                            block_hash: header.block_hash,
                            ..Default::default()
                        },
                    )?;
                }
                Ok(())
            })
            .expect("seed index and headers");
        handle
            .append_and_fanout(finalized_event(&block_2))
            .expect("finalize block 2 (writer path)");

        producer
            .reconcile_finalized_tail()
            .expect("reconciliation succeeds");

        let (start, end, _live) = handle.subscribe(0).unwrap();
        let finalized_for_2 = collect_replay(&handle, start, end)
            .iter()
            .filter(|f| {
                f.kind() == "finalized" && f.block_hash() == Some(block_2.header().block_hash)
            })
            .count();
        assert_eq!(
            finalized_for_2, 1,
            "a frame committed after producer creation must not be re-appended by reconciliation"
        );
    }
}
