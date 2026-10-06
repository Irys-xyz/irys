//! Recall, ingest, migration, and serve drivers.
//!
//! All four share one `StorageModule` and therefore one disk lane. Recall
//! reads use `read_recall_chunks`. Ingest uses `index_transaction_data` and
//! `write_data_chunk_queued`. Migration deposits with `deposit_data_chunks`.
//! Serve uses `read_chunks`.
//!
//! The write path does not check chunk merkle proofs. Payloads are synthetic:
//! a 64-byte data path and a repeated `0x5A` body. `index_transaction_data`
//! also inserts a body-migration row; the driver that indexed the tx settles
//! that row after every chunk of the tx is durable.
//!
//! Durable means `is_data_chunk_durable_at`, which is true only after the
//! packed bytes are synced and the interval is `Data`. The queued-write
//! oneshot fires earlier, at index ack.
//!
//! The offer pacer keeps a deadline. Each release moves that deadline forward
//! by the chunk's interval. It does not start the interval from "now", so a
//! late generator catches up instead of inserting a fresh gap, and a late
//! wake does not stretch the schedule.
//!
//! `--ingest-index serial` (the default) commits each tx on the generator
//! before that tx's chunks are paced. `--ingest-index overlap` starts the
//! next commit while those chunks are paced. Production indexes on the chunk
//! migration service and writes bodies on another task.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eyre::eyre;
use irys_domain::{BatchEnqueueItem, ChunkType, StorageModule};
use irys_types::{
    Base64, DataTransactionHeader, H256, LedgerChunkOffset, LedgerChunkRange, PartitionChunkOffset,
    TxChunkOffset, UnpackedChunk, hash_sha256, ii, ledger_chunk_offset_ii,
    partition_chunk_offset_ii,
};
use rand::rngs::StdRng;
use rand::{Rng as _, SeedableRng as _};
use tokio::sync::{Semaphore, watch};

use crate::schedule::{RECALL_SKIP_LAG_STEPS, Rates, period_ns};
use crate::stats::{Buckets, DriverBucket, PermitGauge, Written};

const BLOCK_HEIGHT: u64 = 1;
const DURABLE_TIMEOUT: Duration = Duration::from_secs(60);
const IDLE_POLL: Duration = Duration::from_millis(20);

pub(crate) struct BenchIo {
    pub module: Arc<StorageModule>,
    pub chunk_size: u64,
    pub partition_chunks: u32,
    pub recall_chunks: u32,
    pub write_origin: u32,
    pub migrate_origin: u32,
    pub write_end: u32,
    pub mix: Vec<(u32, u64)>,
    pub mix_total: u64,
    pub span_limit: usize,
    pub ceiling: u64,
    pub seed: u64,
    pub serve_seq_chunks: u32,
    pub buckets: Mutex<Buckets>,
    pub written: Mutex<Written>,
    pub fatal: Mutex<Option<String>>,
    pub accept_new: AtomicBool,
    pub open_txs: AtomicUsize,
    pub ingest_inflight: AtomicUsize,
    pub migration_inflight: AtomicUsize,
    pub recall_busy: AtomicBool,
    pub serve_busy: AtomicBool,
    pub index_lock: tokio::sync::Mutex<()>,
    pub permits: PermitGauge,
}

pub(crate) fn resume_frontier(module: &StorageModule, start: u32, end: u32) -> u32 {
    let mut lo = start;
    let mut hi = end;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let offset = PartitionChunkOffset::from(mid);
        if module.get_chunk_type(&offset) == Some(ChunkType::Data) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

/// Drop leftover body-migration rows. A resumed run keeps the Data prefix and
/// does not finish a tx that a crashed process indexed only in part.
pub(crate) fn purge_migrations(
    module: &StorageModule,
    partition_chunks: u32,
) -> eyre::Result<usize> {
    if partition_chunks == 0 {
        return Ok(0);
    }
    module.purge_pending_body_migrations_in_range(
        PartitionChunkOffset::from(0_u32),
        PartitionChunkOffset::from(partition_chunks - 1),
    )
}

pub(crate) fn note_fatal(io: &BenchIo, message: impl Into<String>) {
    let message = message.into();
    let mut guard = io
        .fatal
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if guard.is_none() {
        tracing::warn!("{message}");
        *guard = Some(message);
    }
}

pub(crate) fn fatal_reason(io: &BenchIo) -> Option<String> {
    io.fatal
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

pub(crate) fn take_fatal(io: &BenchIo) -> Option<String> {
    io.fatal
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
}

struct OpenTx<'a>(&'a AtomicUsize);

impl<'a> OpenTx<'a> {
    fn enter(counter: &'a AtomicUsize) -> Self {
        counter.fetch_add(1, Ordering::AcqRel);
        Self(counter)
    }
}

impl Drop for OpenTx<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

struct Inflight<'a>(&'a AtomicUsize);

impl Drop for Inflight<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Clears a recall or serve busy flag if the read future is dropped.
struct BusyFlag<'a>(&'a AtomicBool);

impl<'a> BusyFlag<'a> {
    fn hold(flag: &'a AtomicBool) -> Self {
        flag.store(true, Ordering::Release);
        Self(flag)
    }
}

impl Drop for BusyFlag<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

struct Settle {
    left: Arc<AtomicU32>,
    failed: Arc<AtomicBool>,
    module: Arc<StorageModule>,
    io: Arc<BenchIo>,
    kind: Kind,
    key: PartitionChunkOffset,
    root: H256,
}

impl Drop for Settle {
    fn drop(&mut self) {
        let previous = self.left.fetch_sub(1, Ordering::AcqRel);
        if previous != 1 || self.failed.load(Ordering::Acquire) {
            return;
        }
        let started = std::time::Instant::now();
        let settled = self
            .module
            .settle_pending_body_migration(self.key, self.root, BLOCK_HEIGHT);
        record_phase(&self.io, self.kind, PhaseName::Settle, started.elapsed());
        match settled {
            Ok(true) => {}
            Ok(false) => note_fatal(&self.io, "settle found no matching body-migration row"),
            Err(error) => note_fatal(&self.io, format!("settle failed: {error}")),
        }
    }
}

fn stopped(stop: &watch::Receiver<bool>) -> bool {
    *stop.borrow()
}

fn us(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn offer_delay(bytes: u64, mib_s: u64) -> Duration {
    let denom = mib_s.saturating_mul(1024 * 1024);
    if denom == 0 {
        return Duration::ZERO;
    }
    Duration::from_nanos(bytes.saturating_mul(1_000_000_000) / denom)
}

fn keyed(tag: &[u8], index: u64) -> H256 {
    let mut buf = Vec::with_capacity(24 + tag.len());
    buf.extend_from_slice(b"irys-sm-bench-v1");
    buf.extend_from_slice(tag);
    buf.extend_from_slice(&index.to_le_bytes());
    H256::from(hash_sha256(&buf))
}

fn with_bucket(io: &BenchIo, kind: Kind, body: impl FnOnce(&mut DriverBucket)) {
    let mut guard = io
        .buckets
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let bucket = match kind {
        Kind::Recall => &mut guard.recall,
        Kind::Ingest => &mut guard.ingest,
        Kind::Migration => &mut guard.migration,
        Kind::Serve => &mut guard.serve,
    };
    body(bucket);
}

#[derive(Clone, Copy)]
enum Kind {
    Recall,
    Ingest,
    Migration,
    Serve,
}

fn record_enqueued(io: &BenchIo, kind: Kind, bytes: u64) {
    with_bucket(io, kind, |bucket| {
        bucket.enqueued = bucket.enqueued.saturating_add(bytes);
    });
}

fn record_durable(io: &BenchIo, kind: Kind, bytes: u64, latency: Duration) {
    with_bucket(io, kind, |bucket| {
        bucket.add_op(bytes, us(latency));
        bucket.durable = bucket.durable.saturating_add(bytes);
    });
}

/// `waited` is monotonic. The generator is one task, so the sum is wall time
/// spent unable to enqueue, not overlapping worker time.
fn record_enqueue_blocked(io: &BenchIo, kind: Kind, waited: Duration) {
    add_us(io, kind, waited, |bucket| &mut bucket.enqueue_blocked_us);
}

fn record_txreg_wait(io: &BenchIo, kind: Kind, waited: Duration) {
    add_us(io, kind, waited, |bucket| &mut bucket.txreg_wait_us);
}

fn record_gen(io: &BenchIo, kind: Kind, waited: Duration) {
    add_us(io, kind, waited, |bucket| &mut bucket.gen_us);
}

fn record_permit_wait(io: &BenchIo, kind: Kind, waited: Duration) {
    add_us(io, kind, waited, |bucket| &mut bucket.permit_wait_us);
}

fn add_us(
    io: &BenchIo,
    kind: Kind,
    waited: Duration,
    slot: impl FnOnce(&mut DriverBucket) -> &mut u64,
) {
    let micros = us(waited);
    if micros == 0 {
        return;
    }
    with_bucket(io, kind, |bucket| {
        let total = slot(bucket);
        *total = total.saturating_add(micros);
    });
}

enum PhaseName {
    TxReg,
    Settle,
    Enqueue,
    Durable,
}

fn record_phase(io: &BenchIo, kind: Kind, phase: PhaseName, took: Duration) {
    let micros = us(took);
    with_bucket(io, kind, |bucket| {
        let slot = match phase {
            PhaseName::TxReg => &mut bucket.txreg,
            PhaseName::Settle => &mut bucket.settle,
            PhaseName::Enqueue => &mut bucket.enqueue_lat,
            PhaseName::Durable => &mut bucket.durable_wait,
        };
        slot.push(micros);
    });
}

fn record_pace(io: &BenchIo, kind: Kind, behind: Duration, slept: Duration, released: u64) {
    with_bucket(io, kind, |bucket| {
        bucket.pace_behind_us = bucket.pace_behind_us.saturating_add(us(behind));
        bucket.pace_sleep_us = bucket.pace_sleep_us.saturating_add(us(slept));
        bucket.pace_released = bucket.pace_released.saturating_add(released);
    });
}

fn mark_written(io: &BenchIo, offset: u32) {
    io.written
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(offset);
}

/// Offer deadline. `due` is when the next release should happen. `last_active`
/// is when the generator last left this function, so behind-time is wall time
/// and not the sum of each release's lateness.
struct Pacer {
    due: tokio::time::Instant,
    last_active: tokio::time::Instant,
}

impl Pacer {
    fn new() -> Self {
        let now = tokio::time::Instant::now();
        Self {
            due: now,
            last_active: now,
        }
    }

    fn reset(&mut self) {
        let now = tokio::time::Instant::now();
        self.due = now;
        self.last_active = now;
    }
}

/// Time already late, and time to sleep before the deadline.
/// A second call while still catching up counts only the work since `last_active`,
/// not the original lateness again.
fn pace_account(
    due: tokio::time::Instant,
    last_active: tokio::time::Instant,
    now: tokio::time::Instant,
) -> (Duration, Duration) {
    let behind = now.saturating_duration_since(due.max(last_active));
    let sleep = due.saturating_duration_since(now);
    (behind, sleep)
}

/// Release `bytes` on the deadline. The next deadline is `due + delay`, not
/// `now + delay`. While `due` is still in the past the caller does not sleep.
async fn pace_bytes(
    io: &BenchIo,
    kind: Kind,
    pacer: &mut Pacer,
    bytes: u64,
    mib_s: u64,
    stop: &mut watch::Receiver<bool>,
) -> bool {
    if stopped(stop) {
        return false;
    }
    let now = tokio::time::Instant::now();
    let (behind, sleep_for) = pace_account(pacer.due, pacer.last_active, now);
    let mut slept = Duration::ZERO;
    if !sleep_for.is_zero() {
        let started = tokio::time::Instant::now();
        tokio::select! {
            _ = tokio::time::sleep_until(pacer.due) => {}
            _ = stop.changed() => {
                record_pace(io, kind, behind, started.elapsed(), 0);
                pacer.last_active = tokio::time::Instant::now();
                return false;
            }
        }
        slept = started.elapsed();
    }
    if stopped(stop) {
        record_pace(io, kind, behind, slept, 0);
        pacer.last_active = tokio::time::Instant::now();
        return false;
    }
    let delay = offer_delay(bytes, mib_s);
    pacer.due = if delay.is_zero() {
        tokio::time::Instant::now()
    } else {
        pacer.due + delay
    };
    pacer.last_active = tokio::time::Instant::now();
    record_pace(io, kind, behind, slept, bytes);
    true
}

/// Migration pauses while `pending_write_bytes` is above the body-worker ceiling.
/// That wait is enqueue backpressure. Ingest does not use this gate.
async fn wait_ceiling(io: &BenchIo, extra: u64, stop: &watch::Receiver<bool>) -> bool {
    let started = std::time::Instant::now();
    let mut waited = false;
    let allowed = loop {
        if stopped(stop) || fatal_reason(io).is_some() {
            break false;
        }
        let pending = io.module.pending_write_bytes();
        if pending.saturating_add(extra) <= io.ceiling {
            break true;
        }
        waited = true;
        tokio::time::sleep(IDLE_POLL).await;
    };
    if waited {
        record_enqueue_blocked(io, Kind::Migration, started.elapsed());
    }
    allowed
}

enum PermitWait {
    Ready(tokio::sync::OwnedSemaphorePermit),
    Stopped,
}

/// The permit is held until the chunk is durable. Time spent here is the
/// generator waiting on the bench cap, not on `write_data_chunk_queued`.
async fn wait_permit(
    io: &BenchIo,
    kind: Kind,
    permits: &Arc<Semaphore>,
    stop: &mut watch::Receiver<bool>,
) -> eyre::Result<PermitWait> {
    if stopped(stop) {
        return Ok(PermitWait::Stopped);
    }
    let started = std::time::Instant::now();
    tokio::select! {
        permit = permits.clone().acquire_owned() => {
            let waited = started.elapsed();
            record_enqueue_blocked(io, kind, waited);
            record_permit_wait(io, kind, waited);
            if stopped(stop) {
                return Ok(PermitWait::Stopped);
            }
            match permit {
                Ok(permit) => Ok(PermitWait::Ready(permit)),
                Err(_) => Err(eyre!("ingest semaphore closed")),
            }
        }
        _ = stop.changed() => {
            let waited = started.elapsed();
            record_enqueue_blocked(io, kind, waited);
            record_permit_wait(io, kind, waited);
            Ok(PermitWait::Stopped)
        }
    }
}

struct BuiltTx {
    root: H256,
    header: DataTransactionHeader,
    tx_path: Vec<u8>,
    range: LedgerChunkRange,
}

fn build_tx(tag: &[u8], start: u32, chunks: u32, chunk_size: u64) -> BuiltTx {
    let root = keyed(tag, u64::from(start));
    let data_size = u64::from(chunks).saturating_mul(chunk_size);
    let mut header = DataTransactionHeader::default();
    header.data_root = root;
    header.data_size = data_size;
    header.prefix_hash = H256::zero();
    let path_hash = keyed(b"tx-path", u64::from(start));
    let mut tx_path = Vec::with_capacity(64);
    tx_path.extend_from_slice(&path_hash.0);
    tx_path.extend_from_slice(&path_hash.0);
    let end = start + chunks - 1;
    let range = LedgerChunkRange(ledger_chunk_offset_ii!(u64::from(start), u64::from(end)));
    BuiltTx {
        root,
        header,
        tx_path,
        range,
    }
}

fn commit_built(module: &StorageModule, built: &BuiltTx) -> eyre::Result<()> {
    module.index_transaction_data(&built.header, &built.tx_path, built.range, BLOCK_HEIGHT)?;
    Ok(())
}

/// Durable per-tx index commit. The call time includes the bench index lock.
/// Payload build time is `gen`, not this phase.
async fn index_tx_async(
    io: &BenchIo,
    kind: Kind,
    tag: &[u8],
    start: u32,
    chunks: u32,
) -> eyre::Result<H256> {
    let gen_started = std::time::Instant::now();
    let built = build_tx(tag, start, chunks, io.chunk_size);
    record_gen(io, kind, gen_started.elapsed());
    let root = built.root;
    let module = Arc::clone(&io.module);
    let started = std::time::Instant::now();
    let _lock = io.index_lock.lock().await;
    let committed = tokio::task::spawn_blocking(move || commit_built(&module, &built))
        .await
        .map_err(|error| eyre!("index task panicked: {error}"))?;
    record_phase(io, kind, PhaseName::TxReg, started.elapsed());
    committed?;
    Ok(root)
}

struct Prepared {
    start: u32,
    chunks: u32,
    root: H256,
}

/// Keeps `open_txs` elevated from spawn until the commit is visible, so a step
/// drain cannot pass in the window before the body-migration row exists.
struct Prefetch {
    task: Option<tokio::task::JoinHandle<eyre::Result<Prepared>>>,
    io: Arc<BenchIo>,
    armed: bool,
}

impl Prefetch {
    fn spawn(io: &Arc<BenchIo>, start: u32, chunks: u32) -> Self {
        io.open_txs.fetch_add(1, Ordering::AcqRel);
        let task_io = Arc::clone(io);
        Self {
            task: Some(tokio::spawn(async move {
                let root = index_tx_async(&task_io, Kind::Ingest, b"ingest", start, chunks).await?;
                Ok(Prepared {
                    start,
                    chunks,
                    root,
                })
            })),
            io: Arc::clone(io),
            armed: true,
        }
    }

    async fn join(mut self) -> eyre::Result<Prepared> {
        let task = self
            .task
            .take()
            .ok_or_else(|| eyre!("prefetch joined twice"))?;
        task.await
            .map_err(|error| eyre!("prefetch task panicked: {error}"))?
    }
}

impl Drop for Prefetch {
    fn drop(&mut self) {
        if self.armed {
            self.io.open_txs.fetch_sub(1, Ordering::AcqRel);
            self.armed = false;
        }
    }
}

/// Drop an indexed tx the generator will not write. The body-migration row is
/// removed. The data-root and tx-path rows from that commit stay.
async fn undo_prefetch(io: &BenchIo, task: Prefetch) {
    let prepared = match task.join().await {
        Ok(prepared) => prepared,
        Err(error) => {
            note_fatal(io, format!("prefetch index failed: {error}"));
            return;
        }
    };
    let module = Arc::clone(&io.module);
    let key = PartitionChunkOffset::from(prepared.start);
    let root = prepared.root;
    let started = std::time::Instant::now();
    let settled = tokio::task::spawn_blocking(move || {
        module.settle_pending_body_migration(key, root, BLOCK_HEIGHT)
    })
    .await;
    record_phase(io, Kind::Ingest, PhaseName::Settle, started.elapsed());
    match settled {
        Ok(Ok(true)) => {
            tracing::info!(
                start = prepared.start,
                chunks = prepared.chunks,
                "dropped a prefetched ingest tx at a step boundary"
            );
        }
        Ok(Ok(false)) => note_fatal(io, "prefetch settle found no body-migration row"),
        Ok(Err(error)) => note_fatal(io, format!("prefetch settle failed: {error}")),
        Err(error) => note_fatal(io, format!("prefetch settle panicked: {error}")),
    }
}

/// Holds one ingest permit until the chunk task finishes.
struct PermitLease {
    io: Arc<BenchIo>,
}

impl PermitLease {
    fn take(io: &Arc<BenchIo>) -> Self {
        io.permits.acquire();
        Self { io: Arc::clone(io) }
    }
}

impl Drop for PermitLease {
    fn drop(&mut self) {
        self.io.permits.release();
    }
}

fn unpacked(root: H256, data_size: u64, tx_offset: u32, chunk_size: u64) -> UnpackedChunk {
    let mut data_path = Vec::with_capacity(68);
    data_path.extend_from_slice(&root.0);
    data_path.extend_from_slice(&root.0);
    data_path.extend_from_slice(&tx_offset.to_le_bytes());
    UnpackedChunk {
        data_root: root,
        data_size,
        data_path: Base64(data_path),
        bytes: Base64(vec![0x5A; usize::try_from(chunk_size).unwrap_or(0)]),
        tx_offset: TxChunkOffset(tx_offset),
    }
}

async fn wait_durable(module: &StorageModule, offset: PartitionChunkOffset) -> eyre::Result<()> {
    let started = std::time::Instant::now();
    loop {
        if module.is_data_chunk_durable_at(offset) {
            return Ok(());
        }
        if started.elapsed() > DURABLE_TIMEOUT {
            return Err(eyre!(
                "chunk {offset} was not durable within {}s",
                DURABLE_TIMEOUT.as_secs()
            ));
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

fn pick_chunks(rng: &mut StdRng, mix: &[(u32, u64)], total: u64) -> u32 {
    let mut needle = rng.gen_range(0..total.max(1));
    for (chunks, weight) in mix {
        if needle < *weight {
            return *chunks;
        }
        needle -= *weight;
    }
    mix.first().map(|(chunks, _)| *chunks).unwrap_or(1)
}

pub(crate) async fn recall_driver(
    io: Arc<BenchIo>,
    mut rates: watch::Receiver<Rates>,
    mut stop: watch::Receiver<bool>,
) -> eyre::Result<()> {
    let mut rng = StdRng::seed_from_u64(io.seed);
    let mut next_at = tokio::time::Instant::now();
    let mut period_mark = 0_u64;
    loop {
        if stopped(&stop) || fatal_reason(&io).is_some() {
            break;
        }
        let rate = *rates.borrow();
        let Some(period) = period_ns(rate.recall_num, rate.recall_den) else {
            tokio::select! {
                _ = tokio::time::sleep(IDLE_POLL) => {}
                _ = stop.changed() => break,
                _ = rates.changed() => {}
            }
            next_at = tokio::time::Instant::now();
            period_mark = 0;
            continue;
        };
        if period != period_mark {
            next_at = tokio::time::Instant::now();
            period_mark = period;
        }
        let now = tokio::time::Instant::now();
        if now < next_at {
            tokio::select! {
                _ = tokio::time::sleep_until(next_at) => {}
                _ = stop.changed() => break,
                _ = rates.changed() => {}
            }
            continue;
        }
        let delay = now.saturating_duration_since(next_at);
        let late_steps = delay.as_nanos() / u128::from(period);
        if late_steps >= RECALL_SKIP_LAG_STEPS {
            let skipped = u64::try_from(late_steps).unwrap_or(u64::MAX).max(1);
            let delay_us = us(delay);
            with_bucket(&io, Kind::Recall, |bucket| {
                bucket.skips = bucket.skips.saturating_add(skipped);
                bucket.delays_us.push(delay_us);
            });
            next_at = tokio::time::Instant::now();
            continue;
        }
        next_at += Duration::from_nanos(period);
        let delay_us = us(delay);
        let recall_chunks = io.recall_chunks;
        let partition = io.partition_chunks;
        let max_start = partition - recall_chunks;
        let start = rng.gen_range(0..=max_start);
        let module = Arc::clone(&io.module);
        let chunk_size = io.chunk_size;
        let started = std::time::Instant::now();
        let read = {
            let _busy = BusyFlag::hold(&io.recall_busy);
            tokio::task::spawn_blocking(move || {
                let end = start + recall_chunks - 1;
                let range = partition_chunk_offset_ii!(start, end);
                let map = module.read_recall_chunks(range)?;
                let count = map.len();
                if count != usize::try_from(recall_chunks).unwrap_or(usize::MAX) {
                    return Err(eyre!(
                        "recall at {start} returned {count} chunks, expected {recall_chunks}"
                    ));
                }
                Ok(u64::try_from(count)
                    .unwrap_or(u64::MAX)
                    .saturating_mul(chunk_size))
            })
            .await
        };
        let latency = started.elapsed();
        match read {
            Ok(Ok(bytes)) => with_bucket(&io, Kind::Recall, |bucket| {
                bucket.add_op(bytes, us(latency));
                bucket.delays_us.push(delay_us);
            }),
            Ok(Err(error)) => {
                note_fatal(&io, format!("recall read failed: {error}"));
                break;
            }
            Err(error) => {
                note_fatal(&io, format!("recall task panicked: {error}"));
                break;
            }
        }
    }
    Ok(())
}

pub(crate) async fn ingest_driver(
    io: Arc<BenchIo>,
    mut rates: watch::Receiver<Rates>,
    mut stop: watch::Receiver<bool>,
    cursor: u32,
    concurrency: usize,
    overlap_index: bool,
) -> eyre::Result<()> {
    let mut rng = StdRng::seed_from_u64(io.seed.wrapping_add(1));
    let mut next = cursor;
    let mut pacer = Pacer::new();
    // Held until durable, so this bounds in-flight bytes. `write_data_chunk_queued`
    // itself has no concurrency cap. Release is later than production HTTP
    // admission, which returns at index ACK.
    let permits = Arc::new(Semaphore::new(concurrency));
    // At most one tx indexed ahead of the chunk pacer. Serial mode leaves this empty.
    let mut ahead: Option<Prefetch> = None;
    loop {
        if stopped(&stop) || fatal_reason(&io).is_some() {
            if let Some(task) = ahead.take() {
                undo_prefetch(&io, task).await;
            }
            break;
        }
        let rate = *rates.borrow();
        if !io.accept_new.load(Ordering::Acquire) || rate.ingest_mib_s == 0 {
            if let Some(task) = ahead.take() {
                undo_prefetch(&io, task).await;
            }
            tokio::select! {
                _ = tokio::time::sleep(IDLE_POLL) => {}
                _ = stop.changed() => break,
                _ = rates.changed() => {}
            }
            pacer.reset();
            continue;
        }
        let prepared = if let Some(task) = ahead.take() {
            let started = std::time::Instant::now();
            let prepared = match task.join().await {
                Ok(prepared) => prepared,
                Err(error) => {
                    note_fatal(&io, format!("prefetch index failed: {error}"));
                    break;
                }
            };
            record_txreg_wait(&io, Kind::Ingest, started.elapsed());
            prepared
        } else {
            let room = io.migrate_origin.saturating_sub(next);
            if room == 0 {
                note_fatal(&io, "ingest write window is full");
                break;
            }
            let chunks = pick_chunks(&mut rng, &io.mix, io.mix_total).min(room);
            let started = std::time::Instant::now();
            let root = match index_tx_async(&io, Kind::Ingest, b"ingest", next, chunks).await {
                Ok(root) => root,
                Err(error) => {
                    note_fatal(&io, format!("ingest tx at {next}: {error}"));
                    break;
                }
            };
            record_txreg_wait(&io, Kind::Ingest, started.elapsed());
            Prepared {
                start: next,
                chunks,
                root,
            }
        };
        if prepared.start != next {
            note_fatal(
                &io,
                format!(
                    "prefetch start {} is not the ingest frontier {next}",
                    prepared.start
                ),
            );
            break;
        }
        if overlap_index && io.accept_new.load(Ordering::Acquire) && !stopped(&stop) {
            let start = prepared.start.saturating_add(prepared.chunks);
            let room = io.migrate_origin.saturating_sub(start);
            if room > 0 {
                let chunks = pick_chunks(&mut rng, &io.mix, io.mix_total).min(room);
                ahead = Some(Prefetch::spawn(&io, start, chunks));
            }
        }
        let mib_s = rate.ingest_mib_s;
        if let Err(error) = write_tx(
            &io,
            &mut stop,
            Some(&permits),
            Kind::Ingest,
            b"ingest",
            prepared.start,
            prepared.chunks,
            mib_s,
            &mut pacer,
            false,
            Some(prepared.root),
        )
        .await
        {
            note_fatal(&io, format!("ingest tx at {next}: {error}"));
            if let Some(task) = ahead.take() {
                undo_prefetch(&io, task).await;
            }
            break;
        }
        next = next.saturating_add(prepared.chunks);
    }
    Ok(())
}

pub(crate) async fn migration_driver(
    io: Arc<BenchIo>,
    mut rates: watch::Receiver<Rates>,
    mut stop: watch::Receiver<bool>,
    cursor: u32,
) -> eyre::Result<()> {
    let mut next = cursor;
    let mut pacer = Pacer::new();
    let span = u32::try_from(io.span_limit).unwrap_or(u32::MAX).max(1);
    loop {
        if stopped(&stop) || fatal_reason(&io).is_some() {
            break;
        }
        let rate = *rates.borrow();
        if !io.accept_new.load(Ordering::Acquire) || rate.migration_mib_s == 0 {
            tokio::select! {
                _ = tokio::time::sleep(IDLE_POLL) => {}
                _ = stop.changed() => break,
                _ = rates.changed() => {}
            }
            pacer.reset();
            continue;
        }
        let room = io.write_end.saturating_sub(next);
        if room == 0 {
            note_fatal(&io, "migration write window is full");
            break;
        }
        let chunks = span.max(32).min(room);
        let mib_s = rate.migration_mib_s;
        if let Err(error) = write_tx(
            &io,
            &mut stop,
            None,
            Kind::Migration,
            b"migration",
            next,
            chunks,
            mib_s,
            &mut pacer,
            true,
            None,
        )
        .await
        {
            note_fatal(&io, format!("migration tx at {next}: {error}"));
            break;
        }
        next = next.saturating_add(chunks);
    }
    Ok(())
}

async fn write_tx(
    io: &Arc<BenchIo>,
    stop: &mut watch::Receiver<bool>,
    permits: Option<&Arc<Semaphore>>,
    kind: Kind,
    tag: &'static [u8],
    start: u32,
    chunks: u32,
    mib_s: u64,
    pacer: &mut Pacer,
    migration: bool,
    indexed: Option<H256>,
) -> eyre::Result<()> {
    let _open = OpenTx::enter(&io.open_txs);
    let root = if let Some(root) = indexed {
        root
    } else {
        let started = std::time::Instant::now();
        let root = index_tx_async(io, kind, tag, start, chunks).await?;
        record_txreg_wait(io, kind, started.elapsed());
        root
    };
    let failed = Arc::new(AtomicBool::new(false));
    let left = Arc::new(AtomicU32::new(chunks));
    let data_size = u64::from(chunks).saturating_mul(io.chunk_size);
    let mut offset = start;
    let end = start + chunks;
    let span = u32::try_from(io.span_limit).unwrap_or(u32::MAX).max(1);
    while offset < end {
        if stopped(stop) || fatal_reason(io).is_some() {
            failed.store(true, Ordering::Release);
            break;
        }
        let n = if migration { span.min(end - offset) } else { 1 };
        let batch_bytes = u64::from(n).saturating_mul(io.chunk_size);
        if !pace_bytes(io, kind, pacer, batch_bytes, mib_s, stop).await {
            failed.store(true, Ordering::Release);
            break;
        }
        if migration && !wait_ceiling(io, batch_bytes, stop).await {
            failed.store(true, Ordering::Release);
            break;
        }
        let gen_started = std::time::Instant::now();
        let mut batch = Vec::with_capacity(usize::try_from(n).unwrap_or(1));
        for step in 0..n {
            let tx_offset = offset + step - start;
            batch.push(unpacked(root, data_size, tx_offset, io.chunk_size));
        }
        record_gen(io, kind, gen_started.elapsed());
        if migration {
            let module = Arc::clone(&io.module);
            let enqueued_at = std::time::Instant::now();
            let items = tokio::task::spawn_blocking(move || module.deposit_data_chunks(&batch))
                .await
                .map_err(|error| eyre!("deposit task panicked: {error}"))?
                .map_err(|error| eyre!("deposit failed: {error}"))?;
            record_phase(io, kind, PhaseName::Enqueue, enqueued_at.elapsed());
            if !spawn_migration_waits(io, kind, &failed, &left, start, root, n, items)? {
                failed.store(true, Ordering::Release);
                break;
            }
        } else {
            let chunk = batch.pop().ok_or_else(|| eyre!("empty ingest batch"))?;
            let part = PartitionChunkOffset::from(offset);
            let Some(permits) = permits else {
                return Err(eyre!("ingest concurrency cap is missing"));
            };
            let permit = match wait_permit(io, kind, permits, stop).await? {
                PermitWait::Stopped => {
                    failed.store(true, Ordering::Release);
                    break;
                }
                PermitWait::Ready(permit) => permit,
            };
            record_enqueued(io, kind, io.chunk_size);
            io.ingest_inflight.fetch_add(1, Ordering::AcqRel);
            let lease = PermitLease::take(io);
            spawn_ingest_wait(
                Arc::clone(io),
                permit,
                lease,
                kind,
                Arc::clone(&failed),
                Arc::clone(&left),
                start,
                root,
                part,
                chunk,
            );
        }
        offset = offset.saturating_add(n);
    }
    Ok(())
}

fn spawn_migration_waits(
    io: &Arc<BenchIo>,
    kind: Kind,
    failed: &Arc<AtomicBool>,
    left: &Arc<AtomicU32>,
    key: u32,
    root: H256,
    expected: u32,
    items: Vec<BatchEnqueueItem>,
) -> eyre::Result<bool> {
    let mut offsets = Vec::new();
    for item in items {
        match item {
            BatchEnqueueItem::Queued(found) if found.len() == 1 => offsets.push(found[0]),
            BatchEnqueueItem::Queued(_) => {
                failed.store(true, Ordering::Release);
                note_fatal(io, "migration chunk mapped to more than one offset");
                return Ok(false);
            }
            BatchEnqueueItem::NotQueued => {
                failed.store(true, Ordering::Release);
                note_fatal(io, "migration chunk was not queued");
                return Ok(false);
            }
            BatchEnqueueItem::DataRootNotFound => {
                failed.store(true, Ordering::Release);
                note_fatal(io, "migration data root is missing from the index");
                return Ok(false);
            }
        }
    }
    if u32::try_from(offsets.len()).unwrap_or(u32::MAX) != expected {
        failed.store(true, Ordering::Release);
        note_fatal(
            io,
            format!(
                "migration deposit returned {} chunks, expected {expected}",
                offsets.len()
            ),
        );
        return Ok(false);
    }
    for offset in offsets {
        record_enqueued(io, kind, io.chunk_size);
        io.migration_inflight.fetch_add(1, Ordering::AcqRel);
        let io = Arc::clone(io);
        let failed = Arc::clone(failed);
        let left = Arc::clone(left);
        tokio::spawn(async move {
            let _inflight = Inflight(&io.migration_inflight);
            let _settle = Settle {
                left,
                failed: Arc::clone(&failed),
                module: Arc::clone(&io.module),
                io: Arc::clone(&io),
                kind: Kind::Migration,
                key: PartitionChunkOffset::from(key),
                root,
            };
            let started = std::time::Instant::now();
            if let Err(error) = wait_durable(&io.module, offset).await {
                failed.store(true, Ordering::Release);
                note_fatal(&io, format!("migration chunk {offset}: {error}"));
                return;
            }
            record_phase(&io, Kind::Migration, PhaseName::Durable, started.elapsed());
            record_durable(&io, Kind::Migration, io.chunk_size, started.elapsed());
            mark_written(&io, offset.0);
        });
    }
    Ok(true)
}

fn spawn_ingest_wait(
    io: Arc<BenchIo>,
    permit: tokio::sync::OwnedSemaphorePermit,
    lease: PermitLease,
    kind: Kind,
    failed: Arc<AtomicBool>,
    left: Arc<AtomicU32>,
    key: u32,
    root: H256,
    offset: PartitionChunkOffset,
    chunk: UnpackedChunk,
) {
    tokio::spawn(async move {
        let _permit = permit;
        let _lease = lease;
        let _inflight = Inflight(&io.ingest_inflight);
        let _settle = Settle {
            left,
            failed: Arc::clone(&failed),
            module: Arc::clone(&io.module),
            io: Arc::clone(&io),
            kind,
            key: PartitionChunkOffset::from(key),
            root,
        };
        let started = std::time::Instant::now();
        if let Err(error) = io.module.write_data_chunk_queued(&chunk).await {
            failed.store(true, Ordering::Release);
            note_fatal(&io, format!("ingest chunk {offset}: {error}"));
            return;
        }
        record_phase(&io, kind, PhaseName::Enqueue, started.elapsed());
        let durable_at = std::time::Instant::now();
        if let Err(error) = wait_durable(&io.module, offset).await {
            failed.store(true, Ordering::Release);
            note_fatal(&io, format!("ingest chunk {offset}: {error}"));
            return;
        }
        record_phase(&io, kind, PhaseName::Durable, durable_at.elapsed());
        record_durable(&io, kind, io.chunk_size, started.elapsed());
        mark_written(&io, offset.0);
    });
}

pub(crate) async fn serve_driver(
    io: Arc<BenchIo>,
    mut rates: watch::Receiver<Rates>,
    mut stop: watch::Receiver<bool>,
) -> eyre::Result<()> {
    let mut rng = StdRng::seed_from_u64(io.seed.wrapping_add(2));
    let mut cursor = io.write_origin;
    let mut next_at = tokio::time::Instant::now();
    loop {
        if stopped(&stop) || fatal_reason(&io).is_some() {
            break;
        }
        let rate = *rates.borrow();
        if rate.serve_per_sec == 0 {
            tokio::select! {
                _ = tokio::time::sleep(IDLE_POLL) => {}
                _ = stop.changed() => break,
                _ = rates.changed() => {}
            }
            next_at = tokio::time::Instant::now();
            continue;
        }
        let now = tokio::time::Instant::now();
        if now < next_at {
            tokio::select! {
                _ = tokio::time::sleep_until(next_at) => {}
                _ = stop.changed() => break,
                _ = rates.changed() => {}
            }
            continue;
        }
        next_at =
            tokio::time::Instant::now() + Duration::from_nanos(1_000_000_000 / rate.serve_per_sec);
        let sequential = rng.gen_range(0..100) < rate.serve_seq_pct;
        let range = {
            let written = io
                .written
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if written.is_empty() {
                None
            } else if sequential {
                let found = written
                    .run_from(cursor, io.serve_seq_chunks, io.write_end)
                    .or_else(|| {
                        written.run_from(io.write_origin, io.serve_seq_chunks, io.write_end)
                    });
                if let Some((_, end)) = found {
                    cursor = end.saturating_add(1);
                    if cursor >= io.write_end {
                        cursor = io.write_origin;
                    }
                }
                found
            } else {
                let index = rng.gen_range(0..written.len());
                written.get(index).map(|offset| (offset, offset))
            }
        };
        let Some((start, end)) = range else {
            tokio::time::sleep(IDLE_POLL).await;
            continue;
        };
        let module = Arc::clone(&io.module);
        let chunk_size = io.chunk_size;
        let started = std::time::Instant::now();
        let read = {
            let _busy = BusyFlag::hold(&io.serve_busy);
            tokio::task::spawn_blocking(move || {
                let interval = partition_chunk_offset_ii!(start, end);
                let map = module.read_chunks(interval)?;
                if map.is_empty() {
                    return Err(eyre!("serve read {start}..={end} returned no chunks"));
                }
                Ok(u64::try_from(map.len())
                    .unwrap_or(u64::MAX)
                    .saturating_mul(chunk_size))
            })
            .await
        };
        match read {
            Ok(Ok(bytes)) => record_durable(&io, Kind::Serve, bytes, started.elapsed()),
            Ok(Err(error)) => {
                note_fatal(&io, format!("serve read failed: {error}"));
                break;
            }
            Err(error) => {
                note_fatal(&io, format!("serve task panicked: {error}"));
                break;
            }
        }
    }
    Ok(())
}

pub(crate) fn driver_error(
    name: &str,
    joined: Result<eyre::Result<()>, tokio::task::JoinError>,
) -> Option<String> {
    match joined {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(format!("{name} driver failed: {error}")),
        Err(error) => Some(format!("{name} driver panicked: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_late_release_keeps_the_deadline_in_the_past() {
        let start = tokio::time::Instant::now();
        let due = start;
        let last_active = start;
        let now = start + Duration::from_millis(300);
        let (behind, sleep) = pace_account(due, last_active, now);
        assert_eq!(behind, Duration::from_millis(300));
        assert_eq!(sleep, Duration::ZERO);
        let delay = Duration::from_millis(8);
        let next_due = due + delay;
        assert!(next_due < now);

        // The next chunk only counts the work since the previous return.
        let later = now + Duration::from_millis(1);
        let (behind, sleep) = pace_account(next_due, now, later);
        assert_eq!(behind, Duration::from_millis(1));
        assert_eq!(sleep, Duration::ZERO);
    }

    #[test]
    fn an_early_release_sleeps_and_does_not_restart_the_interval_from_now() {
        let start = tokio::time::Instant::now();
        let due = start + Duration::from_millis(100);
        let now = start + Duration::from_millis(40);
        let (behind, sleep) = pace_account(due, start, now);
        assert_eq!(behind, Duration::ZERO);
        assert_eq!(sleep, Duration::from_millis(60));
        let next_due = due + Duration::from_millis(100);
        assert_eq!(
            next_due.saturating_duration_since(start),
            Duration::from_millis(200)
        );
    }
}
