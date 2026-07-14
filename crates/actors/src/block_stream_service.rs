//! The block-stream producer: the single, node-wide writer of the durable `seq` event log.
//!
//! The producer consumes an unbounded feed of [`BlockStreamSignal`]s emitted from the node's
//! authoritative sites (the confirmation loop, the reorg site, and the per-block migration loop).
//! For each signal it builds the wire [`StreamEvent`], appends it to the durable log (assigning
//! the next `seq`), and fans it out to live SSE subscribers. Being the sole writer makes `seq`
//! assignment monotonic and gap-free.
//!
//! The feed is lossless while the node runs — the channel is unbounded and an append failure halts
//! the producer rather than skipping an event — but a signal is in-memory until its append
//! commits, so a crash between a state transition and its append can lose that frame. For
//! `finalized` the durable truth survives in the block index, and startup reconciliation
//! ([`Producer::reconcile_finalized_tail`]) re-derives and appends the missing frames; `observed`
//! and `reorged` frames lost this way are recovered by a follower through the canonical read
//! endpoints, which serve current state rather than transition history.
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
const DEDUP_CAPACITY: NonZeroUsize = match NonZeroUsize::new(10_000) {
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
    /// Live SSE subscribers. The lock is shared with [`Self::append_and_fanout`] so a subscribe's
    /// snapshot+register pair cannot interleave with an append. Bounded senders: a lagging
    /// subscriber is dropped rather than buffered without limit.
    live: Mutex<LiveSubscribers>,
    db: DatabaseProvider,
}

/// The live fan-out registry: subscriber senders plus a `closed` flag the producer sets when it
/// stops appending for good. Once closed, [`BlockStreamHandle::subscribe`] registers no new sender,
/// so a reconnecting follower's SSE ends cleanly after its replay instead of hanging on a live tail
/// nothing will ever feed.
#[derive(Debug)]
struct LiveSubscribers {
    senders: Vec<Sender<Arc<StreamFrame>>>,
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

    /// Atomic replay→live handover: under one lock, snapshot the durable replay bounds `[start, end)` and
    /// register a live sender, so no append interleaves between the snapshot and the registration. The
    /// caller replays `[start, end)` via [`Self::replay_page`] (after the fan-out lock is released), then
    /// tails the live receiver.
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
            live.senders.push(tx);
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

    /// Producer-only. Append `event` to the durable log (assigning `seq`) and fan it out, holding
    /// the same lock as [`Self::subscribe`].
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
        // Drop subscribers whose receiver is closed or lagging past `SUBSCRIBER_BUFFER`; a dropped
        // follower reconnects and replays from the durable log via `subscribe(from_seq)`.
        live.senders.retain(|sender| {
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
                // A failed de-dup rebuild halts the producer (duplicate re-emission would be the
                // alternative); the node itself keeps running with a stale stream, and the halt is
                // surfaced through the `irys.block_stream.halted` gauge.
                let startup_handle = Arc::clone(&producer_handle);
                match Producer::new(producer_handle, chunk_size, shutdown_rx, signal_rx) {
                    Ok(mut producer) => producer.run().await,
                    Err(e) => {
                        error!(error = ?e, "block-stream producer failed to start: de-dup rebuild failed");
                        crate::metrics::record_block_stream_halted();
                        startup_handle.close_live_subscribers();
                    }
                }
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
    ) -> eyre::Result<Self> {
        let (emitted, finalized) = rebuild_state(&handle.db)?;
        Ok(Self {
            handle,
            chunk_size,
            shutdown,
            signal_rx,
            emitted,
            finalized,
            appends_since_prune: 0,
        })
    }

    async fn run(&mut self) {
        info!("block-stream producer started");
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
                maybe_signal = self.signal_rx.recv() => {
                    match maybe_signal {
                        Some(signal) => {
                            if let Err(e) = self.handle_signal(signal) {
                                // A durable append failed: halt rather than silently skip the event
                                // and keep assigning later `seq`s, which would break the lossless-log
                                // contract. Followers reconnect and replay up to the last good `seq`.
                                error!(error = ?e, "block-stream producer halting: durable append failed");
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

    fn drain_queued_signals(&mut self) -> eyre::Result<()> {
        self.signal_rx.close();
        loop {
            match self.signal_rx.try_recv() {
                Ok(signal) => self.handle_signal(signal)?,
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return Ok(()),
            }
        }
    }

    /// Re-derives `finalized` frames lost to a crash between a block's migration commit and the
    /// producer's append (a signal is in-memory until appended). Walks the block index backward
    /// from its tip until it meets a hash the log already finalised, then appends the gap in
    /// ascending height order.
    ///
    /// Runs only when the rebuilt de-dup state holds at least one finalised hash: on a young or
    /// freshly-reset log the index tail predates the log entirely, and "reconciling" it would
    /// emit `finalized` for deep history a follower bootstraps from the canonical reads instead.
    /// The same reasoning caps the backward scan — the crash window loses at most the migration
    /// batch that was in flight, so a scan that runs [`RECONCILE_SCAN_CAP`] deep means the log and
    /// index tails do not meet, and reconciliation aborts rather than replay history.
    fn reconcile_finalized_tail(&mut self) -> eyre::Result<()> {
        if self.finalized.is_empty() {
            return Ok(());
        }
        let missing = self.handle.db.view_eyre(|tx| {
            let mut missing: Vec<(u64, H256)> = Vec::new();
            let Some(latest) = irys_database::block_index_latest_height(tx)? else {
                return Ok(missing);
            };
            for height in (0..=latest).rev() {
                let Some(hash) = irys_database::block_index_hash_by_height(tx, height)? else {
                    break;
                };
                if self.finalized.contains(&hash) {
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
            self.finalized.put(hash, ());
        }
        Ok(())
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
                // An orphaned block may have been migrated (and emitted `finalized`) before this
                // reorg rolled it back. Evict it from the finalized de-dup so that if its fork is
                // later re-adopted and it re-migrates, the fresh `finalized` is emitted rather than
                // suppressed — a follower that demoted it on this frame is waiting for exactly that.
                for block in old_fork.iter() {
                    self.finalized.pop(&block.header().block_hash);
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

/// Rebuilds the producer's in-memory de-dup state (`observed` and `finalized` hashes) from the
/// durable log tail on startup, so a restart does not re-emit for blocks already in the log.
///
/// A failed log read is an error — starting with empty de-dup state would re-emit duplicate
/// frames for every recent block; the caller halts the producer instead. An individual entry that
/// fails to decode is skipped with a warning: unlike the serving path (which must error rather
/// than put a `seq` gap on the wire), the rebuild only mines hashes out of the tail.
fn rebuild_state(db: &DatabaseProvider) -> eyre::Result<(LruCache<H256, ()>, LruCache<H256, ()>)> {
    let mut emitted = LruCache::new(DEDUP_CAPACITY);
    let mut finalized = LruCache::new(DEDUP_CAPACITY);

    // One read tx for both the latest seq and the tail it bounds — an atomic snapshot at startup.
    let events = db.view_eyre(|tx| {
        let Some(latest) = irys_database::block_stream_latest_seq(tx)? else {
            return Ok(Vec::new());
        };
        let capacity = u64::try_from(DEDUP_CAPACITY.get()).unwrap_or(u64::MAX);
        irys_database::read_block_stream_from(tx, latest.saturating_sub(capacity))
    })?;

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
        let mut producer =
            Producer::new(Arc::clone(&handle), 1, shutdown, signal_rx).expect("producer");

        producer.drain_queued_signals().expect("drain signals");

        let (start, end, _live) = handle.subscribe(0).unwrap();
        let frames = collect_replay(&handle, start, end);
        assert_eq!(
            frames.iter().map(|frame| frame.seq).collect::<Vec<_>>(),
            vec![0, 1]
        );
    }

    /// A producer whose signals these tests feed by direct `handle_signal` calls, so the returned
    /// shutdown/sender halves are unused and may drop.
    fn producer_over(handle: &Arc<BlockStreamHandle>) -> Producer {
        let (_shutdown_tx, shutdown) = reth::tasks::shutdown::signal();
        let (_signal_tx, signal_rx) = mpsc::unbounded_channel();
        Producer::new(Arc::clone(handle), 1, shutdown, signal_rx).expect("producer")
    }

    /// The CX-1 regression: a block finalised, then orphaned by a reorg, must emit `finalized`
    /// again when its fork is re-adopted and it re-migrates — live and across a restart's rebuild.
    #[test]
    fn orphaned_block_refinalizes_after_readoption() {
        let (handle, _tmp) = handle_with_events(0);
        let handle = Arc::new(handle);
        let mut producer = producer_over(&handle);

        let block_b = sample_block(1);
        let block_c = sample_block(2);
        let fork_parent = Arc::new(IrysBlockHeader::default());

        producer
            .handle_signal(BlockStreamSignal::Finalized(Arc::clone(&block_b)))
            .expect("finalize B");
        producer
            .handle_signal(BlockStreamSignal::Reorged {
                fork_parent: Arc::clone(&fork_parent),
                old_fork: Arc::new(vec![Arc::clone(&block_b)]),
                new_fork: Arc::new(vec![Arc::clone(&block_c)]),
            })
            .expect("reorg orphaning B");

        // Restart here — between the reorg and the re-migration: the rebuilt de-dup state must
        // mirror the live eviction, so the re-finalise below survives either path.
        let (emitted, finalized) = rebuild_state(&handle.db).expect("rebuild");
        assert!(
            !finalized.contains(&block_b.header().block_hash),
            "rebuild must evict the orphaned hash from the finalized de-dup"
        );
        assert!(emitted.contains(&block_c.header().block_hash));

        producer
            .handle_signal(BlockStreamSignal::Finalized(Arc::clone(&block_b)))
            .expect("re-finalize B after re-adoption");

        let (start, end, _live) = handle.subscribe(0).unwrap();
        let kinds: Vec<&str> = collect_replay(&handle, start, end)
            .iter()
            .map(StreamFrame::kind)
            .collect();
        assert_eq!(
            kinds,
            vec!["finalized", "reorged", "finalized"],
            "the re-migrated orphan must re-emit finalized, not be de-dup-suppressed"
        );

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
        // their finalized signals died with the process before the producer appended them.
        let blocks: Vec<Arc<SealedBlock>> = (1_u64..=3).map(sample_block).collect();
        {
            let mut producer = producer_over(&handle);
            producer
                .handle_signal(BlockStreamSignal::Finalized(Arc::clone(&blocks[0])))
                .expect("finalize block 1");
        }
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
}
