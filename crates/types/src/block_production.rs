use crate::{ChunkDataPath, H256, H256List, IrysAddress, TxPath, partition::PartitionHash};

use serde::{Deserialize, Serialize};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

/// Most tx-path derives that may run at once.
///
/// Each derive reads every tx header in the block. A slow read must not
/// start another derive for every later winning chunk.
const MAX_TX_PATH_DERIVES: usize = 8;

static TX_PATH_DERIVES: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(MAX_TX_PATH_DERIVES)));

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Seed(pub H256);

impl Seed {
    pub fn into_inner(self) -> H256 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SolutionContext {
    pub partition_hash: PartitionHash,
    pub chunk_offset: u32,

    pub mining_address: IrysAddress,
    /// Capacity chunks use [`TxPathSource::Stored`]`(None)`. A data chunk uses
    /// the stored path when the index has one, and a shared derive otherwise.
    pub tx_path: TxPathSource,
    pub data_path: Option<ChunkDataPath>,
    pub chunk: Vec<u8>,
    pub vdf_step: u64,
    pub checkpoints: H256List,
    pub seed: Seed,
    pub solution_hash: H256,
}

/// Where the PoA tx path comes from.
///
/// Schema v2 stores the bytes. An index with no stored path carries one job,
/// started when the solution is found. Every block rebuild awaits that job.
#[derive(Debug, Clone, PartialEq)]
pub enum TxPathSource {
    /// The index stored the path. `None` is a capacity chunk.
    // MDBX-SUBMODULE-INDEX: Stored(Some) is the MDBX data-chunk path; delete that use. Stored(None) stays: capacity chunks on RocksDB still use it.
    Stored(Option<TxPath>),
    /// One derive shared by every rebuild of this solution.
    Derive(TxPathJob),
}

impl Default for TxPathSource {
    fn default() -> Self {
        Self::Stored(None)
    }
}

impl TxPathSource {
    /// Bytes already in memory. A running derive returns `None`.
    pub fn stored_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Stored(Some(bytes)) => Some(bytes),
            Self::Stored(None) | Self::Derive(_) => None,
        }
    }

    /// Stored bytes, or the derive result. The derive runs once.
    pub fn resolve(&self, timeout: Duration) -> Result<Option<TxPath>, TxPathError> {
        match self {
            Self::Stored(path) => Ok(path.clone()),
            Self::Derive(job) => job.wait(timeout).map(Some),
        }
    }

    /// A finished path, or `None` while the derive is still running.
    pub fn try_get(&self) -> Option<Result<Option<TxPath>, TxPathError>> {
        match self {
            Self::Stored(path) => Some(Ok(path.clone())),
            Self::Derive(job) => job.try_get().map(|result| result.map(Some)),
        }
    }
}

/// A derive that has not finished, or its cached result.
#[derive(Clone)]
pub struct TxPathJob {
    slot: Arc<TxPathSlot>,
}

#[derive(Debug)]
struct TxPathSlot {
    state: Mutex<TxPathState>,
    ready: Condvar,
}

#[derive(Debug)]
enum TxPathState {
    Pending,
    Done(Result<TxPath, String>),
}

impl std::fmt::Debug for TxPathJob {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self
            .slot
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &*state {
            TxPathState::Pending => formatter.write_str("TxPathJob(pending)"),
            TxPathState::Done(Ok(path)) => {
                write!(formatter, "TxPathJob(ready {} bytes)", path.len())
            }
            TxPathState::Done(Err(reason)) => write!(formatter, "TxPathJob(failed {reason})"),
        }
    }
}

impl PartialEq for TxPathJob {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.slot, &other.slot)
    }
}

/// The derive did not produce a path that can go into a PoA.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxPathError {
    /// Missing header, root mismatch, leaf mismatch, or a panic in the job.
    Failed(String),
    /// The job was still running when the wait ended.
    TimedOut,
}

impl std::fmt::Display for TxPathError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Failed(reason) => write!(formatter, "{reason}"),
            Self::TimedOut => write!(formatter, "tx path derive timed out"),
        }
    }
}

impl TxPathError {
    /// Label for the drop counter. The log carries the full reason.
    pub fn metric_reason(&self) -> &'static str {
        match self {
            Self::TimedOut => "timeout",
            Self::Failed(reason) if reason.contains("data_root mismatch") => "data_root_mismatch",
            Self::Failed(reason) if reason.contains("placement mismatch") => "placement_mismatch",
            Self::Failed(_) => "failed",
        }
    }
}

impl TxPathJob {
    /// Start `work` on the runtime blocking pool when a derive slot is free.
    ///
    /// `None` when the cap is full or this thread has no Tokio runtime. The
    /// caller skips the chunk. A job that cannot start would stop the recall
    /// scan. The slot is released before waiters see the result.
    pub fn try_spawn<F>(work: F) -> Option<Self>
    where
        F: FnOnce() -> eyre::Result<TxPath> + Send + 'static,
    {
        let permit = Arc::clone(&TX_PATH_DERIVES).try_acquire_owned().ok()?;
        let handle = tokio::runtime::Handle::try_current().ok()?;
        let slot = new_slot();
        let worker = Arc::clone(&slot);
        // Detached. Dropping the handle does not cancel a blocking task.
        drop(handle.spawn_blocking(move || {
            let result = run_derive(work);
            // Free the slot before waiters observe the result. A waiter
            // that starts the next derive must not see this job as still
            // in flight.
            drop(permit);
            finish_job(&worker, result);
        }));
        Some(Self { slot })
    }

    /// A job that already finished. Tests inject a derive result.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn finished(result: eyre::Result<TxPath>) -> Self {
        let slot = new_slot();
        finish_job(&slot, result.map_err(|error| error.to_string()));
        Self { slot }
    }

    /// The finished path, or `None` while the job is still running.
    pub fn try_get(&self) -> Option<Result<TxPath, TxPathError>> {
        let guard = self
            .slot
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &*guard {
            TxPathState::Done(result) => Some(result.clone().map_err(TxPathError::Failed)),
            TxPathState::Pending => None,
        }
    }

    /// Block until the job finishes or `timeout` elapses. Uses monotonic time.
    pub fn wait(&self, timeout: Duration) -> Result<TxPath, TxPathError> {
        let mut guard = self
            .slot
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let deadline = Instant::now() + timeout;
        loop {
            match &*guard {
                TxPathState::Done(result) => {
                    return result.clone().map_err(TxPathError::Failed);
                }
                TxPathState::Pending => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(TxPathError::TimedOut);
                    }
                    let (next, wait) = self
                        .slot
                        .ready
                        .wait_timeout(guard, deadline.saturating_duration_since(now))
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    guard = next;
                    if wait.timed_out() && matches!(*guard, TxPathState::Pending) {
                        return Err(TxPathError::TimedOut);
                    }
                }
            }
        }
    }
}

fn new_slot() -> Arc<TxPathSlot> {
    Arc::new(TxPathSlot {
        state: Mutex::new(TxPathState::Pending),
        ready: Condvar::new(),
    })
}

fn run_derive<F>(work: F) -> Result<TxPath, String>
where
    F: FnOnce() -> eyre::Result<TxPath>,
{
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)) {
        Ok(Ok(path)) => Ok(path),
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => Err("tx path derive panicked".to_owned()),
    }
}

fn finish_job(slot: &TxPathSlot, done: Result<TxPath, String>) {
    let mut guard = slot
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = TxPathState::Done(done);
    slot.ready.notify_all();
}
#[derive(Debug, Clone)]
pub struct Partition {
    pub id: PartitionId,
    pub mining_address: IrysAddress,
}

pub type PartitionId = u64;

impl Default for Partition {
    fn default() -> Self {
        Self {
            id: 0,
            mining_address: IrysAddress::random(),
        }
    }
}

impl Partition {
    pub fn random_with_id(id: u64) -> Self {
        Self {
            id,
            mining_address: IrysAddress::random(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{TxPathError, TxPathJob};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Duration;

    static SPAWN_LOCK: Mutex<()> = Mutex::new(());

    fn lock_derives() -> std::sync::MutexGuard<'static, ()> {
        SPAWN_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn with_runtime<T>(body: impl FnOnce() -> T) -> T {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let _enter = runtime.enter();
        body()
    }

    fn assert_slots_free() {
        assert_eq!(
            super::TX_PATH_DERIVES.available_permits(),
            super::MAX_TX_PATH_DERIVES
        );
    }

    fn closed_gate() -> Arc<(Mutex<bool>, Condvar)> {
        Arc::new((Mutex::new(false), Condvar::new()))
    }

    fn wait_gate(gate: &Arc<(Mutex<bool>, Condvar)>) {
        let (lock, ready) = &**gate;
        let mut open = lock.lock().expect("gate");
        while !*open {
            open = ready.wait(open).expect("gate wait");
        }
    }

    fn release_gate(gate: &Arc<(Mutex<bool>, Condvar)>) {
        let (lock, ready) = &**gate;
        *lock.lock().expect("gate") = true;
        ready.notify_all();
    }

    #[test]
    fn wait_returns_the_same_bytes_twice() {
        let job = TxPathJob::finished(Ok(vec![1, 2, 3]));
        let first = job.wait(Duration::from_secs(2)).expect("first");
        let second = job.wait(Duration::from_secs(2)).expect("second");
        assert_eq!(first, second);
        assert_eq!(first, vec![1, 2, 3]);
    }

    #[test]
    fn wait_times_out_and_the_worker_can_still_finish() {
        let _guard = lock_derives();
        with_runtime(|| {
            let gate = closed_gate();
            let release = Arc::clone(&gate);
            let job = TxPathJob::try_spawn(move || {
                wait_gate(&release);
                Ok(vec![9])
            })
            .expect("started");
            let error = job
                .wait(Duration::from_millis(30))
                .expect_err("still blocked");
            assert_eq!(error, TxPathError::TimedOut);
            release_gate(&gate);
            assert_eq!(job.wait(Duration::from_secs(2)).expect("released"), vec![9]);
            assert_slots_free();
        });
    }

    #[test]
    fn a_failed_job_reports_the_reason() {
        let _guard = lock_derives();
        with_runtime(|| {
            let job = TxPathJob::try_spawn(|| Err(eyre::eyre!("header missing"))).expect("started");
            let error = job.wait(Duration::from_secs(2)).expect_err("failed");
            match error {
                TxPathError::Failed(reason) => {
                    assert!(reason.contains("header missing"), "{reason}");
                }
                TxPathError::TimedOut => panic!("the job finished"),
            }
            assert_slots_free();
        });
    }

    #[test]
    fn metric_reason_splits_mismatch_text() {
        assert_eq!(
            TxPathError::Failed("data_root mismatch: leaf".into()).metric_reason(),
            "data_root_mismatch"
        );
        assert_eq!(
            TxPathError::Failed("placement mismatch: height".into()).metric_reason(),
            "placement_mismatch"
        );
        assert_eq!(
            TxPathError::Failed("header missing".into()).metric_reason(),
            "failed"
        );
        assert_eq!(TxPathError::TimedOut.metric_reason(), "timeout");
    }

    #[test]
    fn try_spawn_refuses_work_past_the_limit() {
        let _guard = lock_derives();
        with_runtime(|| {
            assert_slots_free();
            let gate = closed_gate();
            let mut jobs = Vec::new();
            for _ in 0..super::MAX_TX_PATH_DERIVES {
                let release = Arc::clone(&gate);
                let job = TxPathJob::try_spawn(move || {
                    wait_gate(&release);
                    Ok(vec![1])
                })
                .expect("a free derive slot");
                jobs.push(job);
            }
            assert!(
                TxPathJob::try_spawn(|| Ok(vec![2])).is_none(),
                "one past the cap must not start"
            );
            assert_eq!(super::TX_PATH_DERIVES.available_permits(), 0);
            release_gate(&gate);
            for job in jobs {
                assert_eq!(job.wait(Duration::from_secs(2)).expect("released"), vec![1]);
            }
            assert_slots_free();
            let next =
                TxPathJob::try_spawn(|| Ok(vec![3])).expect("a slot frees when a job finishes");
            assert_eq!(next.wait(Duration::from_secs(2)).expect("next"), vec![3]);
            assert_slots_free();
        });
    }

    #[test]
    fn try_get_is_none_until_the_job_finishes() {
        let _guard = lock_derives();
        with_runtime(|| {
            let gate = closed_gate();
            let release = Arc::clone(&gate);
            let job = TxPathJob::try_spawn(move || {
                wait_gate(&release);
                Ok(vec![4])
            })
            .expect("started");
            assert!(job.try_get().is_none());
            release_gate(&gate);
            assert_eq!(job.wait(Duration::from_secs(2)).expect("released"), vec![4]);
            assert_eq!(job.try_get().expect("done").expect("path"), vec![4]);
            assert_slots_free();
        });
    }

    #[test]
    fn try_spawn_without_a_runtime_returns_none() {
        let _guard = lock_derives();
        assert!(
            TxPathJob::try_spawn(|| Ok(vec![1])).is_none(),
            "no runtime must not hand back a job"
        );
        assert_slots_free();
        with_runtime(|| {
            let (sender, receiver) = std::sync::mpsc::channel();
            let job = TxPathJob::try_spawn(move || {
                sender
                    .send(std::thread::current().name().unwrap_or("").to_owned())
                    .expect("name");
                Ok(vec![7])
            })
            .expect("a later runtime can still derive");
            assert_eq!(job.wait(Duration::from_secs(2)).expect("ran"), vec![7]);
            assert_eq!(receiver.recv().expect("thread name"), "tokio-rt-worker");
            assert_slots_free();
        });
    }

    #[test]
    fn try_spawn_publishes_a_panic_as_a_failed_job() {
        let _guard = lock_derives();
        with_runtime(|| {
            let job = TxPathJob::try_spawn(|| -> eyre::Result<Vec<u8>> { panic!("boom") })
                .expect("the panic is inside the work");
            let error = job.wait(Duration::from_secs(2)).expect_err("failed");
            match error {
                TxPathError::Failed(reason) => assert_eq!(reason, "tx path derive panicked"),
                TxPathError::TimedOut => panic!("the job finished"),
            }
            assert_slots_free();
        });
    }

    #[test]
    fn clones_share_one_derive() {
        let _guard = lock_derives();
        with_runtime(|| {
            let calls = Arc::new(AtomicUsize::new(0));
            let calls_worker = Arc::clone(&calls);
            let job = TxPathJob::try_spawn(move || {
                calls_worker.fetch_add(1, Ordering::SeqCst);
                Ok(vec![5, 6])
            })
            .expect("started");
            let clone = job.clone();
            assert_eq!(job, clone);
            assert_eq!(job.wait(Duration::from_secs(2)).expect("job"), vec![5, 6]);
            assert_eq!(
                clone.wait(Duration::from_secs(2)).expect("clone"),
                vec![5, 6]
            );
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_slots_free();
        });
    }
}
