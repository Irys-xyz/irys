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

use super::{MigrationError, load_chunk_for_migration, write_chunk_to_module};
use irys_database::submodule::tables::PendingBodyMigration;
use irys_domain::{ChunkType, StorageModule, StorageModulesReadGuard};
use irys_types::{
    Config, DataLedger, PartitionChunkOffset, TxChunkOffset, app_state::DatabaseProvider,
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
    /// Modules already warned that a configured `max_pending_write_bytes` was
    /// raised to one flush batch, so the warning fires once, not every pass.
    ceiling_floor_warned: Mutex<HashSet<usize>>,
}

/// What one pass did, summed over every submodule it touched.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PassStats {
    /// Rows outstanding when the pass started.
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
}

impl std::ops::AddAssign for PassStats {
    fn add_assign(&mut self, other: Self) {
        self.rows += other.rows;
        self.written += other.written;
        self.settled += other.settled;
        self.retired += other.retired;
        self.throttled += other.throttled;
        self.unwritable += other.unwritable;
    }
}

/// Outcome of one pass over one job's range within one submodule.
#[derive(Debug)]
enum JobOutcome {
    /// Every offset is durable; delete the row.
    Settled,
    /// Bodies were enqueued or are awaiting flush; keep the row, no penalty.
    Progressed { written: usize },
    /// The pass budget or the pending-write ceiling stopped this job before
    /// its first write; keep the row.
    Deferred,
    /// `unwritable` offsets hold no entropy yet (module packing, or a range
    /// recovery re-marked), so nothing could be written. Wait for the tick;
    /// this is not a stall and never counts against the job.
    Unpacked { unwritable: usize },
    /// Attempted and nothing was written or in flight: `unavailable` offsets
    /// had no local source and `failed` errored (last error kept for the log).
    Stalled {
        unavailable: usize,
        failed: usize,
        last_error: Option<MigrationError>,
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
            ceiling_floor_warned: Mutex::new(HashSet::new()),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_writes_per_pass(mut self, writes_per_pass: usize) -> Self {
        self.writes_per_pass = writes_per_pass;
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
        // Lowest block height among the rows this pass starts from: its distance
        // to the tip is how far the body worker lags the index path.
        let mut oldest_pending_height: Option<u64> = None;
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
            let batches = match sm.pending_body_migration_batches() {
                Ok(batches) => batches,
                Err(error) => {
                    warn!(
                        storage_module.id = sm.id,
                        ?error,
                        "failed to read pending body migrations"
                    );
                    continue;
                }
            };
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
            for (interval, rows) in batches {
                if rows.is_empty() {
                    continue;
                }
                if let Some(min) = rows.iter().map(|(_, job)| job.block_height).min() {
                    oldest_pending_height = Some(oldest_pending_height.map_or(min, |h| h.min(min)));
                }
                let drain = SubmoduleDrain {
                    storage_modules_guard: self.storage_modules_guard.clone(),
                    db: self.db.clone(),
                    config: self.config.clone(),
                    sm: sm.clone(),
                    ledger,
                    interval,
                    rows,
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
            oldest_pending_height.unwrap_or(0),
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
    rows: Vec<(PartitionChunkOffset, PendingBodyMigration)>,
    /// Bodies this task may still enqueue in this pass.
    budget: usize,
    /// `pending_write_bytes` level at which this task stops enqueueing.
    ceiling: u64,
    /// Jobs this task cut short at the ceiling (for `PassStats::throttled`).
    throttled_jobs: usize,
}

impl SubmoduleDrain {
    fn run(mut self) -> PassStats {
        let rows = std::mem::take(&mut self.rows);
        let mut stats = PassStats {
            rows: rows.len(),
            ..PassStats::default()
        };
        for (key, job) in rows {
            match self.drain_job(key, &job) {
                Ok(JobOutcome::Settled) => {
                    if self.settle(key, &job) {
                        stats.settled += 1;
                    }
                }
                Ok(JobOutcome::Progressed { written }) => stats.written += written,
                Ok(JobOutcome::Deferred) => {}
                Ok(JobOutcome::Unpacked { unwritable }) => {
                    stats.unwritable += unwritable;
                    debug!(
                        storage_module.id = self.sm.id,
                        partition.offset = %key,
                        data_root = %job.data_root,
                        unwritable,
                        "body migration job is waiting for entropy at its offsets; no penalty"
                    );
                }
                Ok(JobOutcome::Stalled {
                    unavailable,
                    failed,
                    last_error,
                }) => {
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
        stats.throttled = self.throttled_jobs;
        stats
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
    /// submodule end)]` — writing every offset that is neither durable nor
    /// already queued, within the remaining pass budget and the module's
    /// pending-write ceiling.
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

        let (mut durable, mut in_flight, mut written) = (0_usize, 0_usize, 0_usize);
        let (mut unavailable, mut unwritable, mut failed) = (0_usize, 0_usize, 0_usize);
        let mut last_error = None;
        let mut cut_short = false;
        // Snapshot the module's pending-write level once and track this job's
        // own additions locally: exact enough for a ceiling, and it keeps the
        // `pending_writes` lock out of the per-offset loop.
        let mut pending_bytes = self.sm.pending_write_bytes();
        for partition_offset in first..=last {
            let offset =
                PartitionChunkOffset::from(u32::try_from(partition_offset).map_err(|_| {
                    MigrationError::Other(format!(
                        "partition offset {partition_offset} exceeds u32"
                    ))
                })?);
            if self.sm.is_data_chunk_durable_at(offset) {
                durable += 1;
                continue;
            }
            if self.sm.is_data_write_pending_at(offset) {
                in_flight += 1;
                continue;
            }
            // Only an Entropy offset — on disk, or queued by packing — can take a
            // body: `write_data_chunk` silently writes nothing anywhere else
            // (Uninitialized while packing, Interrupted mid-flush). Skip without
            // touching the cache and let the tick retry once packing lands.
            if !matches!(self.sm.get_chunk_type(&offset), Some(ChunkType::Entropy)) {
                unwritable += 1;
                continue;
            }
            if self.sm.data_writes_paused() {
                // Recovery took the module mid-pass: stop here, keep the row.
                cut_short = true;
                break;
            }
            if pending_bytes >= self.ceiling {
                // Backpressure: the storage service has not flushed what is
                // already queued. Stop for this pass; the row stays and the
                // next pass resumes from the first non-durable offset.
                self.throttled_jobs += 1;
                cut_short = true;
                break;
            }
            if self.budget == 0 {
                cut_short = true;
                break;
            }
            let tx_offset =
                TxChunkOffset::from(u32::try_from(partition_offset - tx_start).map_err(|_| {
                    MigrationError::Other(format!(
                        "tx chunk offset for partition offset {partition_offset} is out of range"
                    ))
                })?);
            match self.write_offset(job, offset, tx_offset) {
                Ok(WriteAttempt::Queued) => {
                    written += 1;
                    self.budget -= 1;
                    pending_bytes += chunk_size;
                }
                Ok(WriteAttempt::NoSource) => unavailable += 1,
                Ok(WriteAttempt::Paused) => {
                    cut_short = true;
                    break;
                }
                Err(error) => {
                    failed += 1;
                    last_error = Some(error);
                }
            }
        }

        let total = usize::try_from(last - first + 1).unwrap_or(usize::MAX);
        Ok(if durable == total {
            JobOutcome::Settled
        } else if written > 0 || in_flight > 0 {
            JobOutcome::Progressed { written }
        } else if cut_short {
            JobOutcome::Deferred
        } else if unavailable == 0 && failed == 0 {
            JobOutcome::Unpacked { unwritable }
        } else {
            JobOutcome::Stalled {
                unavailable,
                failed,
                last_error,
            }
        })
    }

    /// Source and enqueue one body.
    fn write_offset(
        &self,
        job: &PendingBodyMigration,
        offset: PartitionChunkOffset,
        tx_offset: TxChunkOffset,
    ) -> Result<WriteAttempt, MigrationError> {
        let Some(chunk) = load_chunk_for_migration(
            &self.storage_modules_guard,
            &self.db,
            self.ledger,
            job.data_root,
            job.data_size,
            tx_offset,
            &self.config,
        )?
        else {
            return Ok(WriteAttempt::NoSource);
        };
        match write_chunk_to_module(&self.sm, &chunk) {
            Ok(()) => {}
            // The write raced a recovery pause: not a failure of this job.
            Err(_) if self.sm.data_writes_paused() => return Ok(WriteAttempt::Paused),
            Err(error) => return Err(error),
        }
        // `write_data_chunk` decides per placement whether anything was
        // queued; confirm this offset took the body before counting it.
        Ok(
            if self.sm.is_data_write_pending_at(offset) || self.sm.is_data_chunk_durable_at(offset)
            {
                WriteAttempt::Queued
            } else {
                WriteAttempt::NoSource
            },
        )
    }
}

/// Result of trying to source and enqueue one chunk body.
enum WriteAttempt {
    /// The offset now holds a queued (or already durable) data write.
    Queued,
    /// No local source for this body; data sync's problem.
    NoSource,
    /// The module's data writes were paused by recovery; retry next pass.
    Paused,
}
