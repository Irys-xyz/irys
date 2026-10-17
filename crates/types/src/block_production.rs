use crate::{ChunkDataPath, H256, H256List, IrysAddress, TxPath, partition::PartitionHash};

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[cfg(test)]
thread_local! {
    static FAIL_NEXT_TX_PATH_SPAWN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn fail_next_tx_path_spawn() {
    FAIL_NEXT_TX_PATH_SPAWN.with(|flag| flag.set(true));
}

/// Test seam for an OS thread start that fails. Production spawn does not.
fn take_spawn_failure() -> bool {
    #[cfg(test)]
    {
        FAIL_NEXT_TX_PATH_SPAWN.with(|flag| flag.replace(false))
    }
    #[cfg(not(test))]
    {
        false
    }
}

/// Most tx-path derives that may run at once.
///
/// Each derive is its own OS thread plus a consensus read of every tx header
/// in the block. A slow read must not start another thread for every later
/// winning chunk.
const MAX_TX_PATH_DERIVES: usize = 8;

static TX_PATH_DERIVES_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

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
    /// Run `work` on its own thread. The result stays in the job.
    ///
    /// Does not take a derive slot. Tests use this. Mining uses [`Self::try_spawn`].
    pub fn spawn<F>(work: F) -> Self
    where
        F: FnOnce() -> eyre::Result<TxPath> + Send + 'static,
    {
        let slot = new_slot();
        start_worker(Arc::clone(&slot), work);
        Self { slot }
    }

    /// [`Self::spawn`] when fewer than [`MAX_TX_PATH_DERIVES`] jobs are running.
    ///
    /// `None` means this winner must not start another thread. The caller
    /// skips the chunk. That is also the result when the OS cannot create
    /// the thread. The in-flight jobs keep their slots until they finish.
    pub fn try_spawn<F>(work: F) -> Option<Self>
    where
        F: FnOnce() -> eyre::Result<TxPath> + Send + 'static,
    {
        Self::try_spawn_limited(work, &TX_PATH_DERIVES_IN_FLIGHT, MAX_TX_PATH_DERIVES)
    }

    fn try_spawn_limited<F>(work: F, counter: &'static AtomicUsize, limit: usize) -> Option<Self>
    where
        F: FnOnce() -> eyre::Result<TxPath> + Send + 'static,
    {
        let permit = try_permit(counter, limit)?;
        let slot = new_slot();
        let worker = Arc::clone(&slot);
        let started = spawn_derive(permit, worker, work);
        if let Err(error) = started {
            // The worker never ran. A job here would stop the recall scan
            // on a derive that cannot finish.
            finish_job(&slot, Err(error));
            return None;
        }
        Some(Self { slot })
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

fn spawn_derive<F>(permit: DerivePermit, worker: Arc<TxPathSlot>, work: F) -> Result<(), String>
where
    F: FnOnce() -> eyre::Result<TxPath> + Send + 'static,
{
    if take_spawn_failure() {
        // The closure never starts, so it cannot drop the permit.
        drop(permit);
        return Err("thread start failed".to_owned());
    }
    std::thread::Builder::new()
        .name("tx-path-derive".to_owned())
        .spawn(move || {
            let result = run_derive(work);
            // Free the slot before waiters observe the result. A waiter
            // that starts the next derive must not see this job as still
            // in flight.
            drop(permit);
            finish_job(&worker, result);
        })
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn new_slot() -> Arc<TxPathSlot> {
    Arc::new(TxPathSlot {
        state: Mutex::new(TxPathState::Pending),
        ready: Condvar::new(),
    })
}

fn start_worker<F>(slot: Arc<TxPathSlot>, work: F)
where
    F: FnOnce() -> eyre::Result<TxPath> + Send + 'static,
{
    let worker = Arc::clone(&slot);
    let started = std::thread::Builder::new()
        .name("tx-path-derive".to_owned())
        .spawn(move || {
            finish_job(&worker, run_derive(work));
        });
    if let Err(error) = started {
        finish_job(&slot, Err(error.to_string()));
    }
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

struct DerivePermit {
    counter: &'static AtomicUsize,
}

impl Drop for DerivePermit {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::Release);
    }
}

fn try_permit(counter: &'static AtomicUsize, limit: usize) -> Option<DerivePermit> {
    let mut current = counter.load(Ordering::Relaxed);
    loop {
        if current >= limit {
            return None;
        }
        match counter.compare_exchange_weak(
            current,
            current + 1,
            Ordering::AcqRel,
            Ordering::Relaxed,
        ) {
            Ok(_) => {
                return Some(DerivePermit { counter });
            }
            Err(observed) => current = observed,
        }
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
    use super::{TxPathError, TxPathJob, fail_next_tx_path_spawn};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Duration;

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
        let job = TxPathJob::spawn(|| Ok(vec![1, 2, 3]));
        let first = job.wait(Duration::from_secs(2)).expect("first");
        let second = job.wait(Duration::from_secs(2)).expect("second");
        assert_eq!(first, second);
        assert_eq!(first, vec![1, 2, 3]);
    }

    #[test]
    fn wait_times_out_and_the_worker_can_still_finish() {
        let gate = closed_gate();
        let release = Arc::clone(&gate);
        let job = TxPathJob::spawn(move || {
            wait_gate(&release);
            Ok(vec![9])
        });
        let error = job
            .wait(Duration::from_millis(30))
            .expect_err("still blocked");
        assert_eq!(error, TxPathError::TimedOut);
        release_gate(&gate);
        let path = job.wait(Duration::from_secs(2)).expect("released");
        assert_eq!(path, vec![9]);
    }

    #[test]
    fn a_failed_job_reports_the_reason() {
        let job = TxPathJob::spawn(|| Err(eyre::eyre!("header missing")));
        let error = job.wait(Duration::from_secs(2)).expect_err("failed");
        match error {
            TxPathError::Failed(reason) => assert!(reason.contains("header missing"), "{reason}"),
            TxPathError::TimedOut => panic!("the job finished"),
        }
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
        use std::sync::atomic::{AtomicUsize, Ordering};
        static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
        let gate = closed_gate();
        let mut jobs = Vec::new();
        for _ in 0..2 {
            let release = Arc::clone(&gate);
            let job = TxPathJob::try_spawn_limited(
                move || {
                    wait_gate(&release);
                    Ok(vec![1])
                },
                &IN_FLIGHT,
                2,
            )
            .expect("a free derive slot");
            jobs.push(job);
        }
        assert!(
            TxPathJob::try_spawn_limited(|| Ok(vec![2]), &IN_FLIGHT, 2).is_none(),
            "the third derive must not start a thread"
        );
        assert_eq!(IN_FLIGHT.load(Ordering::Relaxed), 2);
        release_gate(&gate);
        for job in jobs {
            assert_eq!(job.wait(Duration::from_secs(2)).expect("released"), vec![1]);
        }
        let next = TxPathJob::try_spawn_limited(|| Ok(vec![3]), &IN_FLIGHT, 2)
            .expect("a slot frees when a job finishes");
        assert_eq!(next.wait(Duration::from_secs(2)).expect("next"), vec![3]);
        assert_eq!(IN_FLIGHT.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn try_get_is_none_until_the_job_finishes() {
        let gate = closed_gate();
        let release = Arc::clone(&gate);
        let job = TxPathJob::spawn(move || {
            wait_gate(&release);
            Ok(vec![4])
        });
        assert!(job.try_get().is_none());
        release_gate(&gate);
        let path = job.wait(Duration::from_secs(2)).expect("released");
        assert_eq!(path, vec![4]);
        assert_eq!(job.try_get().expect("done").expect("path"), vec![4]);
    }

    /// A process that cannot create the worker thread must not hand the miner
    /// a finished job. The recall scan treats `Some` as the solution and stops.
    #[test]
    fn failed_thread_start_does_not_return_a_job() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
        fail_next_tx_path_spawn();
        assert!(
            TxPathJob::try_spawn_limited(|| Ok(vec![1]), &IN_FLIGHT, 1).is_none(),
            "a thread that did not start is not a solution"
        );
        assert_eq!(
            IN_FLIGHT.load(Ordering::SeqCst),
            0,
            "the permit is released when the thread does not start"
        );
        let job = TxPathJob::try_spawn_limited(|| Ok(vec![7]), &IN_FLIGHT, 1)
            .expect("the slot is free after the failed start");
        assert_eq!(job.wait(Duration::from_secs(2)).expect("ran"), vec![7]);
        assert_eq!(IN_FLIGHT.load(Ordering::SeqCst), 0);
    }
}
