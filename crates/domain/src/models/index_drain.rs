//! Batched data-path index commits for one submodule MDBX environment.
//!
//! MDBX chooses the page writes inside a commit. This module only chooses
//! when `update_eyre` starts: after the chunk `pread`s, `pwrite`s, and
//! mining recall on this drive have left the gap, and not during them.

use super::{WriteDataChunkError, disk_lane::DiskWake};
use irys_database::{
    db::IrysDatabaseExt as _,
    submodule::{add_data_path_hash_to_offset_index, add_full_data_path},
};
use irys_types::{ChunkDataPath, ChunkPathHash, PartitionChunkOffset, app_state::DatabaseProvider};
use std::{
    collections::HashSet,
    sync::{
        Arc, Condvar, Mutex, PoisonError,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
};
use tracing::{debug, error};

/// Ops folded into one MDBX transaction when the gap opens.
/// One transaction is one meta write for every op that queued during the
/// chunk run, up to this cap. A larger queue commits again before the next
/// chunk command, still inside that gap.
const MAX_BATCH_SIZE: usize = 512;

/// Shared by every index drain on one storage module and by that module's
/// disk lane. `paused` is the count of chunk runs and mining recalls that
/// still own the spindle.
#[derive(Debug)]
pub(super) struct IndexGap {
    inner: Mutex<GapInner>,
    cv: Condvar,
}

#[derive(Debug)]
struct GapInner {
    paused: u32,
    drains: Vec<Arc<DrainProgress>>,
}

#[derive(Debug)]
struct DrainProgress {
    /// Blocked in `recv`, with no batch in hand.
    at_recv: AtomicBool,
    /// Inside `update_eyre`.
    writing: AtomicBool,
    /// Thread has left `run`, including by panic.
    stopped: AtomicBool,
    /// Shutdown must commit even if a chunk run still holds `paused`.
    release: AtomicBool,
    #[cfg(test)]
    blocked_waits: AtomicU64,
}

/// Decrements `paused` when the chunk run or the recall drops it.
pub(super) struct ChunkBusy {
    gap: Arc<IndexGap>,
}

impl IndexGap {
    pub(super) fn new() -> Self {
        Self {
            inner: Mutex::new(GapInner {
                paused: 0,
                drains: Vec::new(),
            }),
            cv: Condvar::new(),
        }
    }

    fn register(&self) -> Arc<DrainProgress> {
        let progress = Arc::new(DrainProgress {
            at_recv: AtomicBool::new(false),
            writing: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            release: AtomicBool::new(false),
            #[cfg(test)]
            blocked_waits: AtomicU64::new(0),
        });
        self.lock().drains.push(Arc::clone(&progress));
        progress
    }

    /// Block new commits, without waiting for one already inside MDBX.
    /// A mining recall uses this so its `pread` does not wait on the index.
    pub(super) fn pin_recall(self: &Arc<Self>) -> ChunkBusy {
        let mut guard = self.lock();
        guard.paused += 1;
        self.cv.notify_all();
        drop(guard);
        ChunkBusy {
            gap: Arc::clone(self),
        }
    }

    /// Wait until every drain is at `recv` or already paused by a recall,
    /// then hold the gap for one chunk pass.
    pub(super) fn hold_for_chunk_io(self: &Arc<Self>) -> ChunkBusy {
        let mut guard = self.lock();
        loop {
            if guard.paused > 0 || self.drains_idle(&guard) {
                guard.paused += 1;
                return ChunkBusy {
                    gap: Arc::clone(self),
                };
            }
            guard = self.cv.wait(guard).unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn drains_idle(&self, guard: &GapInner) -> bool {
        guard.drains.iter().all(|drain| {
            drain.stopped.load(Ordering::SeqCst)
                || (drain.at_recv.load(Ordering::SeqCst) && !drain.writing.load(Ordering::SeqCst))
        })
    }

    fn end_chunk_busy(&self) {
        let mut guard = self.lock();
        guard.paused -= 1;
        self.cv.notify_all();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, GapInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn notify(&self) {
        let _guard = self.lock();
        self.cv.notify_all();
    }
}

impl Drop for ChunkBusy {
    fn drop(&mut self) {
        self.gap.end_chunk_busy();
    }
}

pub(super) struct IndexOp {
    pub(super) path_hash: ChunkPathHash,
    pub(super) data_path: ChunkDataPath,
    pub(super) offset: PartitionChunkOffset,
    pub(super) generation: u64,
    pub(super) done: mpsc::Sender<Result<(), WriteDataChunkError>>,
    /// Wakes the disk lane once this ack is in the channel. `None` when the
    /// caller has no lane.
    pub(super) wake: Option<DiskWake>,
}

#[derive(Debug)]
struct InFlight {
    offsets: Mutex<HashSet<PartitionChunkOffset>>,
    cv: Condvar,
}

impl Default for InFlight {
    fn default() -> Self {
        Self {
            offsets: Mutex::new(HashSet::new()),
            cv: Condvar::new(),
        }
    }
}

#[derive(Debug)]
pub(super) struct IndexDrain {
    tx: Mutex<Option<mpsc::Sender<IndexOp>>>,
    join: Mutex<Option<JoinHandle<()>>>,
    in_flight: Arc<InFlight>,
    progress: Arc<DrainProgress>,
    gap: Arc<IndexGap>,
}

#[cfg(test)]
pub(super) struct DrainTestHooks {
    pub(super) commit_count: Arc<AtomicU64>,
    pub(super) fail_next: Arc<AtomicBool>,
    pub(super) current_generation: Arc<AtomicU64>,
}

pub(super) struct DrainRunner {
    rx: mpsc::Receiver<IndexOp>,
    db: DatabaseProvider,
    current_generation: Arc<AtomicU64>,
    in_flight: Arc<InFlight>,
    fail_next: Arc<AtomicBool>,
    progress: Arc<DrainProgress>,
    gap: Arc<IndexGap>,
    #[cfg(test)]
    hooks: Option<DrainTestHooks>,
}

impl IndexDrain {
    pub(super) fn spawn(
        db: DatabaseProvider,
        generation: Arc<AtomicU64>,
        fail_next: Arc<AtomicBool>,
        gap: Arc<IndexGap>,
    ) -> eyre::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let in_flight = Arc::new(InFlight::default());
        let progress = gap.register();
        let runner = DrainRunner {
            rx,
            db,
            current_generation: Arc::clone(&generation),
            in_flight: Arc::clone(&in_flight),
            fail_next,
            progress: Arc::clone(&progress),
            gap: Arc::clone(&gap),
            #[cfg(test)]
            hooks: None,
        };
        let join = thread::Builder::new()
            .name("irys-index-drain".into())
            .spawn(move || runner.run())
            .map_err(|error| eyre::eyre!("failed to spawn index drain: {error}"))?;
        Ok(Self {
            tx: Mutex::new(Some(tx)),
            join: Mutex::new(Some(join)),
            in_flight,
            progress,
            gap,
        })
    }

    #[cfg(test)]
    pub(super) fn unstarted(db: DatabaseProvider, hooks: DrainTestHooks) -> (Self, DrainRunner) {
        let gap = Arc::new(IndexGap::new());
        let (tx, rx) = mpsc::channel();
        let in_flight = Arc::new(InFlight::default());
        let progress = gap.register();
        (
            Self {
                tx: Mutex::new(Some(tx)),
                join: Mutex::new(None),
                in_flight: Arc::clone(&in_flight),
                progress: Arc::clone(&progress),
                gap: Arc::clone(&gap),
            },
            DrainRunner {
                rx,
                db,
                current_generation: Arc::clone(&hooks.current_generation),
                in_flight,
                fail_next: Arc::clone(&hooks.fail_next),
                progress,
                gap,
                hooks: Some(hooks),
            },
        )
    }

    pub(super) fn submit(&self, op: IndexOp) {
        // Track before the send. `wait_idle` must see an op that is queued
        // behind a chunk run and not yet inside `update_eyre`.
        self.track(op.offset);
        let tx = self.tx.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(tx) = tx.as_ref() else {
            self.untrack(op.offset);
            send_ack(
                op,
                Err(WriteDataChunkError::Other(eyre::eyre!(
                    "index drain closed"
                ))),
            );
            return;
        };
        if let Err(mpsc::SendError(op)) = tx.send(op) {
            self.untrack(op.offset);
            send_ack(
                op,
                Err(WriteDataChunkError::Other(eyre::eyre!(
                    "index drain closed"
                ))),
            );
        }
    }

    fn track(&self, offset: PartitionChunkOffset) {
        self.in_flight
            .offsets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(offset);
    }

    fn untrack(&self, offset: PartitionChunkOffset) {
        let mut offsets = self
            .in_flight
            .offsets
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        offsets.remove(&offset);
        self.in_flight.cv.notify_all();
    }

    pub(super) fn shutdown(&self) {
        self.progress.release.store(true, Ordering::SeqCst);
        self.gap.notify();
        drop(
            self.tx
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take(),
        );
        let join = self
            .join
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(join) = join
            && join.join().is_err()
        {
            error!("index drain thread panicked");
        }
    }

    pub(super) fn wait_idle(&self) {
        self.wait_idle_in_range(
            PartitionChunkOffset::from(0),
            PartitionChunkOffset::from(u32::MAX),
        );
    }

    pub(super) fn wait_idle_in_range(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) {
        let mut offsets = self
            .in_flight
            .offsets
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        while offsets
            .iter()
            .any(|offset| *offset >= start && *offset <= end)
        {
            offsets = self
                .in_flight
                .cv
                .wait(offsets)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

impl Drop for IndexDrain {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct DrainExit<'a> {
    progress: &'a DrainProgress,
    gap: &'a IndexGap,
}

impl Drop for DrainExit<'_> {
    fn drop(&mut self) {
        self.progress.stopped.store(true, Ordering::SeqCst);
        self.progress.at_recv.store(true, Ordering::SeqCst);
        self.progress.writing.store(false, Ordering::SeqCst);
        self.gap.notify();
    }
}

impl DrainRunner {
    pub(super) fn run(self) {
        let _exit = DrainExit {
            progress: &self.progress,
            gap: &self.gap,
        };
        loop {
            self.arm_recv();
            let first = match self.rx.recv() {
                Ok(op) => op,
                Err(_) => break,
            };
            let mut batch = Vec::with_capacity(MAX_BATCH_SIZE);
            batch.push(first);
            self.fill_batch(&mut batch);
            // A chunk run may still be on the spindle. Wait until it leaves,
            // then pick up ops that arrived during that run.
            self.wait_to_commit();
            self.fill_batch(&mut batch);
            self.commit_batch(&mut batch);
        }
    }

    fn arm_recv(&self) {
        let _guard = self.gap.lock();
        self.progress.writing.store(false, Ordering::SeqCst);
        self.progress.at_recv.store(true, Ordering::SeqCst);
        self.gap.cv.notify_all();
    }

    fn wait_to_commit(&self) {
        let mut guard = self.gap.lock();
        self.progress.at_recv.store(false, Ordering::SeqCst);
        while self.gap_is_paused(&guard) {
            #[cfg(test)]
            {
                self.progress.blocked_waits.fetch_add(1, Ordering::SeqCst);
            }
            guard = self
                .gap
                .cv
                .wait(guard)
                .unwrap_or_else(PoisonError::into_inner);
        }
        self.progress.writing.store(true, Ordering::SeqCst);
    }

    fn gap_is_paused(&self, guard: &GapInner) -> bool {
        guard.paused > 0 && !self.progress.release.load(Ordering::SeqCst)
    }

    fn fill_batch(&self, batch: &mut Vec<IndexOp>) {
        while batch.len() < MAX_BATCH_SIZE {
            match self.rx.try_recv() {
                Ok(op) => batch.push(op),
                Err(_) => break,
            }
        }
    }

    fn commit_batch(&self, batch: &mut Vec<IndexOp>) {
        if batch.is_empty() {
            return;
        }
        let current_generation = self.current_generation.load(Ordering::SeqCst);
        let mut apply = Vec::with_capacity(batch.len());
        let mut skipped = Vec::new();
        for op in batch.drain(..) {
            if op.generation == current_generation {
                apply.push(op);
                continue;
            }
            skipped.push(op);
        }
        if !skipped.is_empty() {
            self.clear_in_flight(&skipped);
            for op in skipped {
                send_ack(op, Err(WriteDataChunkError::WritesPaused));
            }
        }
        if apply.is_empty() {
            return;
        }
        self.mark_in_flight(&apply);
        if self.current_generation.load(Ordering::SeqCst) != current_generation {
            self.clear_in_flight(&apply);
            for op in apply {
                send_ack(op, Err(WriteDataChunkError::WritesPaused));
            }
            return;
        }
        let result = self.write_batch(&apply);
        self.clear_in_flight(&apply);
        for op in apply {
            let ack = match &result {
                Ok(()) => Ok(()),
                Err(error) => Err(WriteDataChunkError::Other(eyre::eyre!("{error}"))),
            };
            send_ack(op, ack);
        }
    }

    fn mark_in_flight(&self, batch: &[IndexOp]) {
        let mut offsets = self
            .in_flight
            .offsets
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        offsets.extend(batch.iter().map(|op| op.offset));
    }

    fn clear_in_flight(&self, batch: &[IndexOp]) {
        let mut offsets = self
            .in_flight
            .offsets
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for op in batch {
            offsets.remove(&op.offset);
        }
        self.in_flight.cv.notify_all();
    }

    fn write_batch(&self, batch: &[IndexOp]) -> eyre::Result<()> {
        #[cfg(test)]
        if let Some(hooks) = &self.hooks {
            hooks.commit_count.fetch_add(1, Ordering::SeqCst);
        }
        if self.fail_next.swap(false, Ordering::SeqCst) {
            eyre::bail!("injected index drain commit failure");
        }
        self.db.update_eyre(|tx| {
            for op in batch {
                // clone: put consumes the path bytes; the IndexOp is kept for the waiter ACK
                add_full_data_path(tx, op.path_hash, op.data_path.clone())?;
                add_data_path_hash_to_offset_index(tx, op.offset, Some(op.path_hash))?;
            }
            Ok(())
        })
    }
}

fn send_ack(op: IndexOp, ack: Result<(), WriteDataChunkError>) {
    if op.done.send(ack).is_err() {
        debug!("index drain waiter dropped");
    }
    // Send first. This notify bumps the epoch under the lane lock, so a lane
    // that has not waited yet still observes the ack.
    if let Some(wake) = &op.wake {
        wake.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::{DrainTestHooks, IndexDrain, IndexGap, IndexOp};
    use crate::WriteDataChunkError;
    use irys_database::{
        IrysDatabaseArgs as _,
        db::IrysDatabaseExt as _,
        submodule::{create_or_open_submodule_db, get_path_hashes_by_offset},
    };
    use irys_testing_utils::utils::TempDirBuilder;
    use irys_types::{PartitionChunkOffset, UnpackedChunk, app_state::DatabaseProvider};
    use reth_db::mdbx::DatabaseArguments;
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicU64, Ordering},
            mpsc,
        },
        thread,
        time::{Duration, Instant},
    };

    fn open_submodule_db(path: &std::path::Path) -> eyre::Result<DatabaseProvider> {
        let env = create_or_open_submodule_db(path, DatabaseArguments::irys_testing()?)?;
        Ok(DatabaseProvider(Arc::new(env)))
    }

    fn submit_op(
        drain: &IndexDrain,
        offset: u32,
        data_path: Vec<u8>,
        generation: u64,
    ) -> mpsc::Receiver<Result<(), WriteDataChunkError>> {
        let (done_tx, done_rx) = mpsc::channel();
        drain.submit(IndexOp {
            path_hash: UnpackedChunk::hash_data_path(&data_path),
            data_path,
            offset: PartitionChunkOffset::from(offset),
            generation,
            done: done_tx,
            wake: None,
        });
        done_rx
    }

    fn path_hash_at(
        db: &DatabaseProvider,
        offset: u32,
    ) -> eyre::Result<Option<irys_types::ChunkPathHash>> {
        db.view_eyre(|tx| {
            Ok(
                get_path_hashes_by_offset(tx, PartitionChunkOffset::from(offset))?
                    .and_then(|hashes| hashes.data_path_hash),
            )
        })
    }

    #[test]
    fn two_ops_queued_before_drain_share_one_commit() -> eyre::Result<()> {
        let tmp = TempDirBuilder::new()
            .prefix("index_drain_share_commit")
            .with_tracing()
            .build();
        let db = open_submodule_db(&tmp.path().join("db"))?;
        let commit_count = Arc::new(AtomicU64::new(0));
        let (drain, runner) = IndexDrain::unstarted(
            db.clone(),
            DrainTestHooks {
                commit_count: Arc::clone(&commit_count),
                fail_next: Arc::new(AtomicBool::new(false)),
                current_generation: Arc::new(AtomicU64::new(0)),
            },
        );

        let first_path = vec![1_u8, 2, 3];
        let second_path = vec![4_u8, 5, 6];
        let first_hash = UnpackedChunk::hash_data_path(&first_path);
        let second_hash = UnpackedChunk::hash_data_path(&second_path);
        let first_done = submit_op(&drain, 0, first_path, 0);
        let second_done = submit_op(&drain, 1, second_path, 0);
        drop(drain);

        runner.run();

        assert_eq!(commit_count.load(Ordering::SeqCst), 1);
        first_done.recv()?.map_err(|err| eyre::eyre!("{err}"))?;
        second_done.recv()?.map_err(|err| eyre::eyre!("{err}"))?;
        assert_eq!(path_hash_at(&db, 0)?, Some(first_hash));
        assert_eq!(path_hash_at(&db, 1)?, Some(second_hash));
        Ok(())
    }

    #[test]
    fn failed_commit_fails_every_waiter() -> eyre::Result<()> {
        let tmp = TempDirBuilder::new()
            .prefix("index_drain_shared_failure")
            .with_tracing()
            .build();
        let db = open_submodule_db(&tmp.path().join("db"))?;
        let commit_count = Arc::new(AtomicU64::new(0));
        let fail_next = Arc::new(AtomicBool::new(true));
        let (drain, runner) = IndexDrain::unstarted(
            db.clone(),
            DrainTestHooks {
                commit_count: Arc::clone(&commit_count),
                fail_next: Arc::clone(&fail_next),
                current_generation: Arc::new(AtomicU64::new(0)),
            },
        );

        let first_done = submit_op(&drain, 0, vec![1_u8, 2, 3], 0);
        let second_done = submit_op(&drain, 1, vec![4_u8, 5, 6], 0);
        drop(drain);

        runner.run();

        assert!(first_done.recv()?.is_err());
        assert!(second_done.recv()?.is_err());
        assert_eq!(path_hash_at(&db, 0)?, None);
        assert_eq!(path_hash_at(&db, 1)?, None);
        Ok(())
    }

    #[test]
    fn queued_write_is_dropped_across_pause_clear_resume() -> eyre::Result<()> {
        let tmp = TempDirBuilder::new()
            .prefix("index_drain_pause_generation")
            .with_tracing()
            .build();
        let db = open_submodule_db(&tmp.path().join("db"))?;
        let current_generation = Arc::new(AtomicU64::new(1));
        let path = vec![7_u8, 8, 9];
        let (drain, runner) = IndexDrain::unstarted(
            db.clone(),
            DrainTestHooks {
                commit_count: Arc::new(AtomicU64::new(0)),
                fail_next: Arc::new(AtomicBool::new(false)),
                current_generation: Arc::clone(&current_generation),
            },
        );
        let done = submit_op(&drain, 0, path, 1);
        current_generation.store(2, Ordering::SeqCst);
        drop(drain);
        runner.run();

        assert!(matches!(
            done.recv()?,
            Err(WriteDataChunkError::WritesPaused)
        ));
        assert_eq!(path_hash_at(&db, 0)?, None);

        let (drain, runner) = IndexDrain::unstarted(
            db.clone(),
            DrainTestHooks {
                commit_count: Arc::new(AtomicU64::new(0)),
                fail_next: Arc::new(AtomicBool::new(false)),
                current_generation: Arc::clone(&current_generation),
            },
        );
        let resume_path = vec![9_u8, 8, 7];
        let resume_hash = UnpackedChunk::hash_data_path(&resume_path);
        let resumed = submit_op(&drain, 0, resume_path, 2);
        drop(drain);
        runner.run();
        resumed.recv()?.map_err(|err| eyre::eyre!("{err}"))?;
        assert_eq!(path_hash_at(&db, 0)?, Some(resume_hash));
        Ok(())
    }

    #[test]
    fn commit_waits_while_the_chunk_hold_is_set() -> eyre::Result<()> {
        let tmp = TempDirBuilder::new()
            .prefix("index_drain_chunk_hold")
            .with_tracing()
            .build();
        let db = open_submodule_db(&tmp.path().join("db"))?;
        let commit_count = Arc::new(AtomicU64::new(0));
        let (drain, runner) = IndexDrain::unstarted(
            db.clone(),
            DrainTestHooks {
                commit_count: Arc::clone(&commit_count),
                fail_next: Arc::new(AtomicBool::new(false)),
                current_generation: Arc::new(AtomicU64::new(0)),
            },
        );
        let worker = thread::spawn(move || runner.run());
        let started = Instant::now();
        while !drain.progress.at_recv.load(Ordering::SeqCst) {
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "index drain did not reach recv"
            );
            thread::yield_now();
        }
        let hold = drain.gap.hold_for_chunk_io();
        let first_path = vec![1_u8, 2, 3];
        let second_path = vec![4_u8, 5, 6];
        let first_hash = UnpackedChunk::hash_data_path(&first_path);
        let second_hash = UnpackedChunk::hash_data_path(&second_path);
        let first_done = submit_op(&drain, 0, first_path, 0);
        let second_done = submit_op(&drain, 1, second_path, 0);
        let started = Instant::now();
        while drain.progress.blocked_waits.load(Ordering::SeqCst) == 0 {
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "index drain committed during the chunk hold"
            );
            thread::yield_now();
        }
        assert_eq!(commit_count.load(Ordering::SeqCst), 0);
        drop(hold);
        drop(drain);
        worker.join().expect("index drain");
        assert_eq!(commit_count.load(Ordering::SeqCst), 1);
        first_done.recv()?.map_err(|err| eyre::eyre!("{err}"))?;
        second_done.recv()?.map_err(|err| eyre::eyre!("{err}"))?;
        assert_eq!(path_hash_at(&db, 0)?, Some(first_hash));
        assert_eq!(path_hash_at(&db, 1)?, Some(second_hash));
        Ok(())
    }

    #[test]
    fn chunk_hold_returns_while_a_recall_pin_is_held() {
        let gap = Arc::new(IndexGap::new());
        let _progress = gap.register();
        let _pin = gap.pin_recall();
        let waiting = Arc::clone(&gap);
        let (entered_tx, entered_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let _hold = waiting.hold_for_chunk_io();
            entered_tx.send(()).expect("hold entered");
        });
        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("chunk hold waited while a recall pin was set");
        worker.join().expect("hold thread");
    }
}
