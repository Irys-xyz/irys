//! Background drain for `PendingBodyMigrationsByOffset`.
//!
//! `on_block_migrated` commits chunk **indexes** in block order and returns. The
//! chunk **bodies** those indexes describe are copied out of the chunk cache
//! (or, for Publish, the durable Submit replica) by this worker at whatever
//! speed the target disk allows. A huge or slow body write therefore never
//! delays the next block's indexes — the failure that froze n3's Publish
//! ledger behind a single 4 GiB Submit tx.
//!
//! The rows are the backlog of record: they survive restarts and outlive a
//! dropped wake-up. Every submodule is its own MDBX env and its own IO domain,
//! so a pass fans out one blocking task per submodule with outstanding rows.

use super::{MigrationError, load_chunks_for_migration, write_chunks_to_module};
use irys_database::submodule::tables::PendingBodyMigration;
use irys_domain::{BatchEnqueueItem, ChunkType, StorageModule, StorageModulesReadGuard};
use irys_types::{
    Config, DataLedger, PartitionChunkOffset, TxChunkOffset, UnpackedChunk,
    app_state::DatabaseProvider,
};
use nodit::Interval;
use reth::tasks::shutdown::Shutdown;
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::Notify;
use tracing::{debug, info, warn};

/// Fallback wake-up so a missed notification only costs latency.
pub(crate) const BODY_MIGRATION_TICK: Duration = Duration::from_secs(2);

/// Consecutive passes that attempted a job and wrote nothing before it is
/// retired and its remaining holes left to data sync.
pub(crate) const MAX_BODY_MIGRATION_ATTEMPTS: u32 = 8;

/// Chunk bodies enqueued per submodule per pass. Pacing, not abandonment: the
/// row stays and the next pass continues where this one stopped. Bounds
/// `pending_writes` growth to roughly this × chunk_size per submodule between
/// storage-service flushes.
pub(crate) const BODY_WRITES_PER_PASS: usize = 512;

/// Pending rows read per page. A pass holds one page and reads the next key
/// only while write budget remains. The pending key moves to the first offset
/// that is not durable, so the next pass does not walk that prefix.
pub(crate) const BODY_ROWS_PER_PASS: usize = 64;

/// Default pending-write ceiling, in flush batches per submodule: one batch
/// the storage service is flushing plus one the worker is filling. Overridden
/// by `storage.max_pending_write_bytes`.
pub(crate) const PENDING_WRITE_CEILING_BATCHES: u64 = 2;

/// Floor on the derived ceiling, in chunks per submodule (64 MiB at 256 KiB).
/// A small `num_writes_before_sync` (tests use 1) must not throttle the worker
/// to a couple of chunks per flush tick.
pub(crate) const PENDING_WRITE_CEILING_MIN_CHUNKS: u64 = 256;

/// Re-check interval after a pass was cut short by the ceiling: the storage
/// service flushes on a 1s tick, so wait for that rather than a full
/// [`BODY_MIGRATION_TICK`].
pub(crate) const BODY_MIGRATION_THROTTLED_TICK: Duration = Duration::from_millis(250);

/// Bytes a storage module may hold in `pending_writes` before the worker stops
/// writing to it for the rest of the pass. The per-pass budget bounds the
/// *rate* of enqueueing; this bounds the *level*, so a storage-service flush
/// tick that falls behind can never let memory grow without limit.
pub(crate) fn pending_write_ceiling_bytes(config: &Config, sm: &StorageModule) -> u64 {
    let storage = &config.node_config.storage;
    let submodules = sm.submodule_count() as u64;
    // One flush batch per submodule. Below this level the storage service's
    // threshold flush never fires and only its 5s idle force-flush drains
    // writes, so a configured ceiling is never allowed under it.
    let one_batch = storage
        .num_writes_before_sync
        .saturating_mul(config.consensus.chunk_size)
        .saturating_mul(submodules);
    match storage.max_pending_write_bytes {
        Some(configured) => configured.max(one_batch),
        None => PENDING_WRITE_CEILING_BATCHES
            .saturating_mul(storage.num_writes_before_sync)
            .max(PENDING_WRITE_CEILING_MIN_CHUNKS)
            .saturating_mul(config.consensus.chunk_size)
            .saturating_mul(submodules),
    }
}

pub(crate) struct BodyMigrationWorker {
    storage_modules_guard: StorageModulesReadGuard,
    db: DatabaseProvider,
    config: Config,
    wakeup: Arc<Notify>,
    shutdown: Shutdown,
    writes_per_pass: usize,
    rows_per_pass: usize,
    /// Modules already warned that a configured `max_pending_write_bytes` was
    /// raised to one flush batch, so the warning fires once, not every pass.
    ceiling_floor_warned: Mutex<HashSet<usize>>,
}

/// What one pass did, summed over every submodule it touched.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PassStats {
    /// Jobs this pass started. A pass that stops on the write budget has not
    /// read the jobs past that page, so this is not the full backlog.
    pub rows: usize,
    /// Chunk bodies enqueued to `pending_writes`.
    pub written: usize,
    /// Rows deleted because every offset was durable.
    pub settled: usize,
    /// Rows deleted after `MAX_BODY_MIGRATION_ATTEMPTS` no-progress passes.
    pub retired: usize,
    /// Jobs cut short because their module hit the pending-write ceiling.
    pub throttled: usize,
    /// Offsets skipped because they hold no entropy yet (module still packing,
    /// or a range recovery just re-marked). They wait; they are not failures.
    pub unwritable: usize,
    /// Lowest block height among the jobs this pass read. `None` when it
    /// read none. A pass that stops early has not seen later keys.
    pub oldest_pending_height: Option<u64>,
}

impl std::ops::AddAssign for PassStats {
    fn add_assign(&mut self, other: Self) {
        self.rows += other.rows;
        self.written += other.written;
        self.settled += other.settled;
        self.retired += other.retired;
        self.throttled += other.throttled;
        self.unwritable += other.unwritable;
        self.oldest_pending_height = match (self.oldest_pending_height, other.oldest_pending_height)
        {
            (Some(left), Some(right)) => Some(left.min(right)),
            (Some(height), None) | (None, Some(height)) => Some(height),
            (None, None) => None,
        };
    }
}

/// Outcome of one pass over one job's range within one submodule.
#[derive(Debug)]
enum JobOutcome {
    /// Every offset is durable; delete the row.
    Settled,
    /// Bodies were enqueued or are awaiting flush; keep the row, no penalty.
    /// `resume` is the first offset that is not durable.
    Progressed {
        written: usize,
        resume: PartitionChunkOffset,
    },
    /// The pass budget or the pending-write ceiling stopped this job before
    /// its first write; keep the row. `resume` is the first offset that is
    /// not durable.
    Deferred { resume: PartitionChunkOffset },
    /// `unwritable` offsets hold no entropy yet (module packing, or a range
    /// recovery re-marked), so nothing could be written. Wait for the tick;
    /// this is not a stall and never counts against the job.
    Unpacked {
        unwritable: usize,
        resume: PartitionChunkOffset,
    },
    /// Attempted and nothing was written or in flight: `unavailable` offsets
    /// had no local source and `failed` errored (last error kept for the log).
    Stalled {
        unavailable: usize,
        failed: usize,
        last_error: Option<MigrationError>,
        resume: PartitionChunkOffset,
    },
}

impl BodyMigrationWorker {
    pub(crate) fn new(
        storage_modules_guard: StorageModulesReadGuard,
        db: DatabaseProvider,
        config: Config,
        wakeup: Arc<Notify>,
        shutdown: Shutdown,
    ) -> Self {
        Self {
            storage_modules_guard,
            db,
            config,
            wakeup,
            shutdown,
            writes_per_pass: BODY_WRITES_PER_PASS,
            rows_per_pass: BODY_ROWS_PER_PASS,
            ceiling_floor_warned: Mutex::new(HashSet::new()),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_writes_per_pass(mut self, writes_per_pass: usize) -> Self {
        self.writes_per_pass = writes_per_pass;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_rows_per_pass(mut self, rows_per_pass: usize) -> Self {
        self.rows_per_pass = rows_per_pass.max(1);
        self
    }

    pub(crate) async fn run(mut self) {
        info!("service started: chunk_migration body worker");
        let wakeup = self.wakeup.clone();
        loop {
            // Drain back-to-back while passes make progress. The first
            // iteration is the startup drain of rows a previous process left.
            // A pass that writes nothing means the backlog is empty, every
            // outstanding offset is unsourceable for now, or the module is at
            // its pending-write ceiling and needs the storage service's flush
            // tick — all of which want a wait, not a spin.
            let throttled = loop {
                let stats = self.drain_pass().await;
                if stats.written == 0 || self.shutdown_requested() {
                    break stats.throttled > 0;
                }
                tokio::task::yield_now().await;
            };
            let wait = if throttled {
                BODY_MIGRATION_THROTTLED_TICK
            } else {
                BODY_MIGRATION_TICK
            };
            tokio::select! {
                biased;
                _ = &mut self.shutdown => break,
                _ = tokio::time::timeout(wait, wakeup.notified()) => {}
            }
        }
        info!("shutting down chunk_migration body worker");
    }

    fn shutdown_requested(&mut self) -> bool {
        use futures::FutureExt as _;
        (&mut self.shutdown).now_or_never().is_some()
    }

    /// One pass over every ledger-assigned storage module: each submodule with
    /// outstanding rows is drained on its own blocking task, in parallel.
    pub(crate) async fn drain_pass(&self) -> PassStats {
        let modules: Vec<Arc<StorageModule>> = self.storage_modules_guard.read().clone();
        let mut tasks = Vec::new();
        for sm in modules {
            let Some(ledger) = sm
                .partition_assignment()
                .and_then(|pa| pa.ledger_id)
                .and_then(|id| DataLedger::try_from(id).ok())
            else {
                continue;
            };
            if sm.data_writes_paused() {
                // Network-partition recovery is rewriting this module; its rows
                // wait untouched until writes resume.
                debug!(
                    storage_module.id = sm.id,
                    "body migration skipping module with paused writes"
                );
                continue;
            }
            let ceiling = pending_write_ceiling_bytes(&self.config, &sm);
            if let Some(configured) = self.config.node_config.storage.max_pending_write_bytes
                && configured < ceiling
                && self.ceiling_floor_warned.lock().unwrap().insert(sm.id)
            {
                warn!(
                    storage_module.id = sm.id,
                    configured,
                    ceiling,
                    "storage.max_pending_write_bytes is below one flush batch for this module; \
                     using one flush batch instead (a lower ceiling would stall body migration \
                     on the storage service's idle flush)"
                );
            }
            for interval in sm.submodule_intervals() {
                // One row tells us the submodule has work. The task pages the rest.
                let nonempty = match sm.pending_body_migrations_page(
                    PartitionChunkOffset::from(*interval.start()),
                    None,
                    1,
                ) {
                    Ok(rows) => !rows.is_empty(),
                    Err(error) => {
                        warn!(
                            storage_module.id = sm.id,
                            ?error,
                            "failed to read pending body migrations"
                        );
                        continue;
                    }
                };
                if !nonempty {
                    continue;
                }
                let drain = SubmoduleDrain {
                    storage_modules_guard: self.storage_modules_guard.clone(),
                    db: self.db.clone(),
                    config: self.config.clone(),
                    sm: sm.clone(),
                    ledger,
                    interval,
                    rows_per_pass: self.rows_per_pass,
                    budget: self.writes_per_pass,
                    ceiling,
                    throttled_jobs: 0,
                };
                tasks.push(tokio::task::spawn_blocking(move || drain.run()));
            }
        }

        let mut total = PassStats::default();
        for joined in futures::future::join_all(tasks).await {
            match joined {
                Ok(stats) => total += stats,
                Err(error) => warn!(?error, "body migration drain task panicked"),
            }
        }
        if total.rows > 0 {
            debug!(?total, "body migration pass");
        }
        crate::metrics::record_body_migration_pass(
            total.written as u64,
            total.settled as u64,
            total.retired as u64,
            total.throttled as u64,
            total.rows.saturating_sub(total.settled + total.retired) as u64,
            total.oldest_pending_height.unwrap_or(0),
        );
        total
    }
}

/// Everything one blocking task needs to drain one submodule's rows.
struct SubmoduleDrain {
    storage_modules_guard: StorageModulesReadGuard,
    db: DatabaseProvider,
    config: Config,
    sm: Arc<StorageModule>,
    ledger: DataLedger,
    interval: Interval<PartitionChunkOffset>,
    /// Pending rows this task loads per read.
    rows_per_pass: usize,
    /// Bodies this task may still enqueue in this pass.
    budget: usize,
    /// `pending_write_bytes` level at which this task stops enqueueing.
    ceiling: u64,
    /// Jobs this task cut short at the ceiling (for `PassStats::throttled`).
    throttled_jobs: usize,
}

impl SubmoduleDrain {
    fn run(mut self) -> PassStats {
        let mut stats = PassStats::default();
        // A move inside this pass puts the same job at a later key. Skip it
        // when a later page reads that key; this pass already walked it.
        let mut seen = HashSet::new();
        let mut cursor = None;
        loop {
            if self.budget == 0 {
                break;
            }
            let page = match self.sm.pending_body_migrations_page(
                PartitionChunkOffset::from(*self.interval.start()),
                cursor,
                self.rows_per_pass,
            ) {
                Ok(page) => page,
                Err(error) => {
                    warn!(
                        storage_module.id = self.sm.id,
                        ?error,
                        "failed to read pending body migrations"
                    );
                    break;
                }
            };
            if page.is_empty() {
                break;
            }
            let page_len = page.len();
            let last_key = page.last().expect("page is not empty").0;
            for (mut key, job) in page {
                if self.budget == 0 {
                    break;
                }
                if !seen.insert((job.data_root, job.block_height, job.start_offset.0)) {
                    continue;
                }
                stats.rows += 1;
                stats.oldest_pending_height = Some(
                    stats
                        .oldest_pending_height
                        .map_or(job.block_height, |height| height.min(job.block_height)),
                );
                match self.drain_job(key, &job) {
                    Ok(outcome) => {
                        key = self.persisted_key(key, &job, &outcome);
                        match outcome {
                            JobOutcome::Settled => {
                                if self.settle(key, &job) {
                                    stats.settled += 1;
                                }
                            }
                            JobOutcome::Progressed { written, .. } => stats.written += written,
                            JobOutcome::Deferred { .. } => {}
                            JobOutcome::Unpacked { unwritable, .. } => {
                                stats.unwritable += unwritable;
                                debug!(
                                    storage_module.id = self.sm.id,
                                    partition.offset = %key,
                                    data_root = %job.data_root,
                                    unwritable,
                                    "body migration job is waiting for entropy at its offsets; no penalty"
                                );
                            }
                            JobOutcome::Stalled {
                                unavailable,
                                failed,
                                last_error,
                                ..
                            } => {
                                debug!(
                                    storage_module.id = self.sm.id,
                                    partition.offset = %key,
                                    data_root = %job.data_root,
                                    unavailable,
                                    failed,
                                    ?last_error,
                                    "body migration job made no progress; leaving offsets for data sync"
                                );
                                self.record_stall(key, &job, &mut stats);
                            }
                        }
                    }
                    Err(error) => {
                        warn!(
                            storage_module.id = self.sm.id,
                            partition.offset = %key,
                            data_root = %job.data_root,
                            ?error,
                            "body migration job failed; counting as a stalled pass"
                        );
                        self.record_stall(key, &job, &mut stats);
                    }
                }
            }
            if self.budget == 0 || page_len < self.rows_per_pass {
                break;
            }
            let Some(next) = last_key.0.checked_add(1) else {
                break;
            };
            cursor = Some(PartitionChunkOffset::from(next));
        }
        stats.throttled = self.throttled_jobs;
        stats
    }

    /// Move the row to the first offset that is not durable, in one batch.
    /// `Settled` keeps the key so the delete hits the row that is still there.
    /// A failed move leaves the old key; the next pass repeats the prefix.
    fn persisted_key(
        &self,
        key: PartitionChunkOffset,
        job: &PendingBodyMigration,
        outcome: &JobOutcome,
    ) -> PartitionChunkOffset {
        let resume = match outcome {
            JobOutcome::Settled => return key,
            JobOutcome::Progressed { resume, .. }
            | JobOutcome::Deferred { resume }
            | JobOutcome::Unpacked { resume, .. }
            | JobOutcome::Stalled { resume, .. } => *resume,
        };
        if resume == key {
            return key;
        }
        match self.sm.advance_pending_body_migration(key, resume, job) {
            Ok(stored) => stored,
            Err(error) => {
                warn!(
                    storage_module.id = self.sm.id,
                    partition.offset = %key,
                    resume.offset = %resume,
                    ?error,
                    "failed to advance body migration job; the next pass repeats the prefix"
                );
                key
            }
        }
    }

    /// One no-progress pass on `job`: bump its attempts and retire it once the
    /// limit is reached, so an unsourceable or persistently failing row cannot
    /// be retried forever.
    fn record_stall(
        &self,
        key: PartitionChunkOffset,
        job: &PendingBodyMigration,
        stats: &mut PassStats,
    ) {
        match self
            .sm
            .bump_pending_body_migration_attempts(key, job.data_root, job.block_height)
        {
            Ok(Some(attempts)) if attempts >= MAX_BODY_MIGRATION_ATTEMPTS => {
                warn!(
                    storage_module.id = self.sm.id,
                    partition.offset = %key,
                    data_root = %job.data_root,
                    attempts,
                    "retiring body migration job after repeated passes without progress; \
                     remaining holes left to data sync"
                );
                if self.settle(key, job) {
                    stats.retired += 1;
                }
            }
            Ok(_) => {}
            Err(error) => warn!(
                storage_module.id = self.sm.id,
                partition.offset = %key,
                ?error,
                "failed to record body migration attempt"
            ),
        }
    }

    fn settle(&self, key: PartitionChunkOffset, job: &PendingBodyMigration) -> bool {
        match self
            .sm
            .settle_pending_body_migration(key, job.data_root, job.block_height)
        {
            Ok(removed) => removed,
            Err(error) => {
                warn!(
                    storage_module.id = self.sm.id,
                    partition.offset = %key,
                    ?error,
                    "failed to settle body migration job"
                );
                false
            }
        }
    }

    /// Walk the slice of `job` this submodule owns — `[key, min(tx end,
    /// submodule end)]` — and publish each contiguous writable run in one
    /// sweep-queue insert. A durable, pending, non-entropy, or unsourced
    /// offset ends the run. A run of entropy offsets that still need a body
    /// is one source read. A run also ends at one entropy sweep, and at
    /// the pass budget or the pending-write ceiling.
    fn drain_job(
        &mut self,
        key: PartitionChunkOffset,
        job: &PendingBodyMigration,
    ) -> Result<JobOutcome, MigrationError> {
        let chunk_size = self.config.consensus.chunk_size;
        let num_chunks = i64::try_from(job.data_size.div_ceil(chunk_size)).map_err(|_| {
            MigrationError::Other(format!(
                "data_size {} exceeds i64 chunk count",
                job.data_size
            ))
        })?;
        let tx_start = i64::from(*job.start_offset);
        let first = i64::from(*key);
        let last = (tx_start + num_chunks - 1).min(i64::from(*self.interval.end()));
        if last < first {
            return Ok(JobOutcome::Settled);
        }

        let (mut durable, mut in_flight, mut unwritable) = (0_usize, 0_usize, 0_usize);
        // First offset that is not durable. The row moves here when the
        // prefix before it is durable. In-flight writes are not durable, so
        // the key does not pass them.
        let mut first_incomplete: Option<PartitionChunkOffset> = None;
        let mut progress = JobProgress {
            written: 0,
            // Snapshot the module's pending-write level once and track this
            // job's own additions locally: exact enough for a ceiling, and it
            // keeps the `pending_writes` lock out of the per-offset loop.
            pending_bytes: self.sm.pending_write_bytes(),
            unavailable: 0,
            failed: 0,
            last_error: None,
        };
        let mut cut_short = false;
        let span_limit = span_chunk_limit(
            chunk_size,
            self.config.node_config.storage.entropy_sweep_max_bytes,
        );
        let mut run: Vec<LoadedBody> = Vec::new();
        let mut cursor = first;
        while cursor <= last {
            let partition_offset = cursor;
            let offset =
                PartitionChunkOffset::from(u32::try_from(partition_offset).map_err(|_| {
                    MigrationError::Other(format!(
                        "partition offset {partition_offset} exceeds u32"
                    ))
                })?);
            if self.sm.is_data_chunk_durable_at(offset) {
                if self.flush_run(&mut run, &mut progress)? {
                    cut_short = true;
                    break;
                }
                durable += 1;
                cursor += 1;
                continue;
            }
            if first_incomplete.is_none() {
                first_incomplete = Some(offset);
            }
            if self.sm.is_data_write_pending_at(offset) {
                if self.flush_run(&mut run, &mut progress)? {
                    cut_short = true;
                    break;
                }
                in_flight += 1;
                cursor += 1;
                continue;
            }
            // Only an Entropy offset — on disk, or queued by packing — can take a
            // body: `write_data_chunk` silently writes nothing anywhere else
            // (Uninitialized while packing, Interrupted mid-flush). Skip without
            // touching the cache and let the tick retry once packing lands.
            if !matches!(self.sm.get_chunk_type(&offset), Some(ChunkType::Entropy)) {
                if self.flush_run(&mut run, &mut progress)? {
                    cut_short = true;
                    break;
                }
                unwritable += 1;
                cursor += 1;
                continue;
            }
            if self.sm.data_writes_paused() {
                // Recovery took the module mid-pass: stop here, keep the row.
                let _ = self.flush_run(&mut run, &mut progress)?;
                cut_short = true;
                break;
            }
            if !run.is_empty()
                && (run.len() >= span_limit || run.len() >= self.budget)
                && self.flush_run(&mut run, &mut progress)?
            {
                cut_short = true;
                break;
            }
            if self.budget == 0 {
                cut_short = true;
                break;
            }
            let next_level = progress.pending_bytes.saturating_add(
                (run.len() as u64)
                    .saturating_add(1)
                    .saturating_mul(chunk_size),
            );
            if next_level > self.ceiling {
                // Backpressure: the storage service has not flushed what is
                // already queued. Stop for this pass. The row's key moves to
                // the first offset that is not durable.
                if self.flush_run(&mut run, &mut progress)? {
                    cut_short = true;
                    break;
                }
                self.throttled_jobs += 1;
                cut_short = true;
                break;
            }
            let mut window_last = partition_offset;
            let mut look = partition_offset + 1;
            while look <= last {
                let ahead = (window_last - partition_offset + 1) as usize;
                if run.len() + ahead >= span_limit || run.len() + ahead >= self.budget {
                    break;
                }
                let look_offset =
                    PartitionChunkOffset::from(u32::try_from(look).map_err(|_| {
                        MigrationError::Other(format!("partition offset {look} exceeds u32"))
                    })?);
                if self.sm.is_data_chunk_durable_at(look_offset)
                    || self.sm.is_data_write_pending_at(look_offset)
                    || !matches!(
                        self.sm.get_chunk_type(&look_offset),
                        Some(ChunkType::Entropy)
                    )
                {
                    break;
                }
                let look_level = progress.pending_bytes.saturating_add(
                    (run.len() as u64)
                        .saturating_add(ahead as u64)
                        .saturating_add(1)
                        .saturating_mul(chunk_size),
                );
                if look_level > self.ceiling {
                    break;
                }
                window_last = look;
                look += 1;
            }
            let count = (window_last - partition_offset + 1) as usize;
            let tx_offset =
                TxChunkOffset::from(u32::try_from(partition_offset - tx_start).map_err(|_| {
                    MigrationError::Other(format!(
                        "tx chunk offset for partition offset {partition_offset} is out of range"
                    ))
                })?);
            let bodies = match load_chunks_for_migration(
                &self.storage_modules_guard,
                &self.db,
                self.ledger,
                job.data_root,
                job.data_size,
                tx_offset,
                count,
                &self.config,
            ) {
                Ok(bodies) => bodies,
                Err(MigrationError::WritesPaused) => {
                    let _ = self.flush_run(&mut run, &mut progress)?;
                    cut_short = true;
                    break;
                }
                Err(error) => {
                    if self.flush_run(&mut run, &mut progress)? {
                        cut_short = true;
                        break;
                    }
                    progress.failed += count;
                    progress.last_error = Some(error);
                    cursor = window_last + 1;
                    continue;
                }
            };
            let mut resume = window_last + 1;
            for (step, body) in bodies.into_iter().enumerate() {
                if !run.is_empty()
                    && (run.len() >= span_limit || run.len() >= self.budget)
                    && self.flush_run(&mut run, &mut progress)?
                {
                    cut_short = true;
                    resume = partition_offset + step as i64;
                    break;
                }
                if self.budget == 0 {
                    cut_short = true;
                    resume = partition_offset + step as i64;
                    break;
                }
                let step_level = progress.pending_bytes.saturating_add(
                    (run.len() as u64)
                        .saturating_add(1)
                        .saturating_mul(chunk_size),
                );
                if step_level > self.ceiling {
                    let _ = self.flush_run(&mut run, &mut progress)?;
                    self.throttled_jobs += 1;
                    cut_short = true;
                    resume = partition_offset + step as i64;
                    break;
                }
                let at = PartitionChunkOffset::from(
                    u32::try_from(partition_offset + step as i64).map_err(|_| {
                        MigrationError::Other(format!(
                            "partition offset {} exceeds u32",
                            partition_offset + step as i64
                        ))
                    })?,
                );
                match body {
                    Some(chunk) => run.push(LoadedBody { offset: at, chunk }),
                    None => {
                        if self.flush_run(&mut run, &mut progress)? {
                            cut_short = true;
                            resume = partition_offset + step as i64 + 1;
                            break;
                        }
                        progress.unavailable += 1;
                    }
                }
            }
            cursor = resume;
            if cut_short {
                break;
            }
        }
        if self.flush_run(&mut run, &mut progress)? {
            cut_short = true;
        }

        let written = progress.written;
        let unavailable = progress.unavailable;
        let failed = progress.failed;
        let last_error = progress.last_error;
        let total = usize::try_from(last - first + 1).unwrap_or(usize::MAX);
        let resume = first_incomplete.unwrap_or(key);
        // `first_incomplete` stays empty only when every offset in the span
        // was durable. A stop in the middle has already recorded the hole.
        Ok(if durable == total && first_incomplete.is_none() {
            JobOutcome::Settled
        } else if written > 0 || in_flight > 0 {
            JobOutcome::Progressed { written, resume }
        } else if cut_short {
            JobOutcome::Deferred { resume }
        } else if unavailable == 0 && failed == 0 {
            JobOutcome::Unpacked { unwritable, resume }
        } else {
            JobOutcome::Stalled {
                unavailable,
                failed,
                last_error,
                resume,
            }
        })
    }

    /// Publish `run` as one sweep-queue insert. `Ok(true)` means recovery
    /// paused the module and the bodies were not queued.
    fn flush_run(
        &mut self,
        run: &mut Vec<LoadedBody>,
        progress: &mut JobProgress,
    ) -> Result<bool, MigrationError> {
        if run.is_empty() {
            return Ok(false);
        }
        let bodies = std::mem::take(run);
        let chunks: Vec<_> = bodies.iter().map(|body| body.chunk.clone()).collect();
        let items = match write_chunks_to_module(&self.sm, &chunks) {
            Ok(items) => items,
            Err(MigrationError::WritesPaused) => return Ok(true),
            Err(error) => {
                progress.failed += bodies.len();
                progress.last_error = Some(error);
                return Ok(false);
            }
        };
        let chunk_size = self.config.consensus.chunk_size;
        for (body, item) in bodies.into_iter().zip(items) {
            let missing_root = matches!(&item, BatchEnqueueItem::DataRootNotFound);
            let queued = match &item {
                BatchEnqueueItem::Queued(offsets) => {
                    offsets.contains(&body.offset)
                        || self.sm.is_data_write_pending_at(body.offset)
                        || self.sm.is_data_chunk_durable_at(body.offset)
                }
                BatchEnqueueItem::NotQueued => {
                    self.sm.is_data_write_pending_at(body.offset)
                        || self.sm.is_data_chunk_durable_at(body.offset)
                }
                BatchEnqueueItem::DataRootNotFound => false,
            };
            if queued {
                progress.written += 1;
                self.budget = self.budget.saturating_sub(1);
                progress.pending_bytes = progress.pending_bytes.saturating_add(chunk_size);
            } else if missing_root {
                progress.failed += 1;
                progress.last_error = Some(MigrationError::ChunkDataWrite);
            } else {
                progress.unavailable += 1;
            }
        }
        Ok(false)
    }
}

struct LoadedBody {
    offset: PartitionChunkOffset,
    chunk: UnpackedChunk,
}

struct JobProgress {
    written: usize,
    pending_bytes: u64,
    unavailable: usize,
    failed: usize,
    last_error: Option<MigrationError>,
}

fn span_chunk_limit(chunk_size: u64, sweep_max_bytes: u64) -> usize {
    let chunk = chunk_size.max(1);
    usize::try_from(sweep_max_bytes.max(chunk) / chunk)
        .unwrap_or(1)
        .max(1)
}
