//! Opt-in group commit for one submodule index.
//!
//! The group stays off until a caller enables it. A registration is visible
//! when its write returns. MDBX syncs every `txs_per_sync` registrations, on
//! a later durable write, on an explicit sync, or 50 ms after the first
//! unsynced write. RocksDB ignores that count. The next synced write on that
//! database, the configured backstop, or clean shutdown makes the WAL
//! durable. The clock is [`Instant`]: this bound is in-process only.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use eyre::WrapErr as _;

/// How long a quiet MDBX index may leave a registration unsynced.
///
/// Also the pause after a failed sync, so a zero Rocks backstop cannot spin.
const DELAY: Duration = Duration::from_millis(50);

/// How long [`GroupCommit::wait_until_synced`] waits after the group stops.
const STOP_WAIT: Duration = Duration::from_secs(5);

/// What `prepare` tells the store to do.
pub(super) enum EnableStep {
    /// Start the delay thread, then arm the group.
    Spawn,
    /// The delay thread is already running. Arm the group.
    Arm,
    /// Already on. `txs_per_sync` was updated.
    On,
}

/// Shared commit counter for every clone of one submodule index.
pub(super) struct GroupCommit {
    enabled: AtomicBool,
    syncs: AtomicU64,
    /// Synced WAL writes. RocksDB only. Includes backstop syncs.
    #[cfg_attr(not(feature = "rocksdb"), allow(dead_code))]
    wal_syncs: AtomicU64,
    /// Backstop thread, explicit `sync_group`, and shutdown WAL syncs.
    #[cfg_attr(not(feature = "rocksdb"), allow(dead_code))]
    backstop_syncs: AtomicU64,
    state: Mutex<State>,
    wake: Condvar,
}

struct State {
    stop: bool,
    /// A delay thread has been spawned. Stays set so a failed arm does not
    /// start a second thread.
    spawned: bool,
    txs_per_sync: u32,
    /// Visible registrations since the group was created.
    generation: u64,
    /// Registrations covered by a later sync.
    synced_generation: u64,
    /// First unsynced registration in the open group. The delay bound starts
    /// here and does not move forward when more writes arrive.
    dirty_since: Option<Instant>,
    /// How long a quiet group waits. MDBX stays at [`DELAY`]. RocksDB sets
    /// this from `index_wal_backstop_ms` before the delay thread starts.
    delay: Duration,
    /// [`GroupCommit::set_delay`] has run. Rocks enable uses its default
    /// only when this is false, so a caller-supplied bound is kept.
    #[cfg_attr(not(feature = "rocksdb"), allow(dead_code))]
    delay_set: bool,
}

impl GroupCommit {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            enabled: AtomicBool::new(false),
            syncs: AtomicU64::new(0),
            wal_syncs: AtomicU64::new(0),
            backstop_syncs: AtomicU64::new(0),
            state: Mutex::new(State {
                stop: false,
                spawned: false,
                txs_per_sync: 1,
                generation: 0,
                synced_generation: 0,
                dirty_since: None,
                delay: DELAY,
                delay_set: false,
            }),
            wake: Condvar::new(),
        })
    }

    pub(super) fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(super) fn sync_count(&self) -> u64 {
        self.syncs.load(Ordering::Relaxed)
    }

    #[cfg_attr(not(feature = "rocksdb"), allow(dead_code))]
    pub(super) fn wal_syncs(&self) -> u64 {
        self.wal_syncs.load(Ordering::Relaxed)
    }

    #[cfg_attr(not(feature = "rocksdb"), allow(dead_code))]
    pub(super) fn backstop_syncs(&self) -> u64 {
        self.backstop_syncs.load(Ordering::Relaxed)
    }

    /// One `set_sync(true)` write or `flush_wal(true)` that is not the backstop.
    #[cfg_attr(not(feature = "rocksdb"), allow(dead_code))]
    pub(super) fn note_wal_sync(&self) {
        self.wal_syncs.fetch_add(1, Ordering::Relaxed);
    }

    /// One backstop, explicit group sync, or shutdown `flush_wal(true)`.
    ///
    /// Also counts as a WAL sync. A synced data write that covers the open
    /// group does not call this.
    #[cfg_attr(not(feature = "rocksdb"), allow(dead_code))]
    pub(super) fn note_backstop_sync(&self) {
        self.wal_syncs.fetch_add(1, Ordering::Relaxed);
        self.backstop_syncs.fetch_add(1, Ordering::Relaxed);
    }

    /// Replace the quiet-group bound and wake the delay thread.
    ///
    /// `Duration::ZERO` makes the thread sync as soon as it wakes. It does
    /// not sync on the caller thread.
    #[cfg_attr(not(feature = "rocksdb"), allow(dead_code))]
    pub(super) fn set_delay(&self, delay: Duration) {
        let mut state = self.lock();
        state.delay = delay;
        state.delay_set = true;
        drop(state);
        self.wake.notify_all();
    }

    #[cfg_attr(not(feature = "rocksdb"), allow(dead_code))]
    pub(super) fn delay_is_set(&self) -> bool {
        self.lock().delay_set
    }

    #[cfg(all(test, feature = "rocksdb"))]
    pub(super) fn delay(&self) -> Duration {
        self.lock().delay
    }

    pub(super) fn prepare(&self, txs_per_sync: u32) -> eyre::Result<EnableStep> {
        if txs_per_sync == 0 {
            eyre::bail!("index group commit txs_per_sync must be at least 1");
        }
        let mut state = self.lock();
        if state.stop {
            eyre::bail!("index group commit is stopped");
        }
        state.txs_per_sync = txs_per_sync;
        if self.is_enabled() {
            return Ok(EnableStep::On);
        }
        if state.spawned {
            return Ok(EnableStep::Arm);
        }
        state.spawned = true;
        Ok(EnableStep::Spawn)
    }

    pub(super) fn clear_spawned(&self) {
        let mut state = self.lock();
        if !self.is_enabled() {
            state.spawned = false;
        }
    }

    pub(super) fn finish_enable(&self) {
        self.enabled.store(true, Ordering::Release);
    }

    /// Record one visible registration. `true` when the group is full.
    ///
    /// MDBX syncs when this returns `true`. RocksDB ignores the bool: the
    /// next synced write or the backstop covers the record.
    pub(super) fn note_visible(&self) -> bool {
        let mut state = self.lock();
        state.generation = state.generation.wrapping_add(1);
        if state.dirty_since.is_none() {
            state.dirty_since = Some(Instant::now());
        }
        let outstanding = state.generation.wrapping_sub(state.synced_generation);
        let due = outstanding >= u64::from(state.txs_per_sync);
        drop(state);
        self.wake.notify_all();
        due
    }

    pub(super) fn generation(&self) -> u64 {
        self.lock().generation
    }

    pub(super) fn synced_generation(&self) -> u64 {
        self.lock().synced_generation
    }

    /// Block until `target` is covered.
    ///
    /// The caller must not hold the index write lock: the sync that covers
    /// `target` takes that lock. After stop, a failed final sync returns
    /// within [`STOP_WAIT`] instead of waiting forever.
    pub(super) fn wait_until_synced(&self, target: u64) -> eyre::Result<()> {
        let mut state = self.lock();
        let mut stop_deadline = None;
        loop {
            if state.synced_generation >= target {
                return Ok(());
            }
            if state.stop {
                let deadline = *stop_deadline.get_or_insert_with(|| Instant::now() + STOP_WAIT);
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    eyre::bail!("index sync stopped before generation {target} became durable");
                }
                let (guard, timeout) = self
                    .wake
                    .wait_timeout(state, left)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state = guard;
                if timeout.timed_out() && state.synced_generation < target {
                    eyre::bail!("index sync stopped before generation {target} became durable");
                }
                continue;
            }
            state = self
                .wake
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// Generation a sync should cover, when any registration is still unsynced.
    pub(super) fn dirty_generation(&self) -> Option<u64> {
        let state = self.lock();
        if state.generation == state.synced_generation {
            None
        } else {
            Some(state.generation)
        }
    }

    /// A synced write already made `covered` durable. Does not count a group sync.
    ///
    /// A generation newer than `covered` stays dirty. The sample is taken
    /// before the sync, so a registration that lands during the sync is not
    /// marked durable.
    pub(super) fn mark_synced(&self, covered: u64, count: bool) {
        let mut state = self.lock();
        if covered > state.synced_generation {
            state.synced_generation = covered;
        }
        if state.generation == state.synced_generation {
            state.dirty_since = None;
        }
        drop(state);
        if count {
            self.syncs.fetch_add(1, Ordering::Relaxed);
        }
        self.wake.notify_all();
    }

    /// The caller just did its own synced write, which also covers the open group.
    ///
    /// Rocks only. A synced `WriteBatch` already flushed the WAL, so this does
    /// not sync again and does not count a backstop sync. MDBX commits are
    /// not synced that way.
    #[cfg(feature = "rocksdb")]
    pub(super) fn cover_durable(&self) {
        if !self.is_enabled() {
            return;
        }
        let mut state = self.lock();
        state.synced_generation = state.generation;
        state.dirty_since = None;
        drop(state);
        self.wake.notify_all();
    }

    pub(super) fn request_stop(&self) {
        let mut state = self.lock();
        state.stop = true;
        drop(state);
        self.wake.notify_all();
    }

    /// `false` when the group is stopping. `true` when the delay has elapsed.
    pub(super) fn wait_until_due(&self) -> bool {
        let mut state = self.lock();
        loop {
            if state.stop {
                return false;
            }
            if let Some(since) = state.dirty_since {
                let left = state.delay.saturating_sub(since.elapsed());
                if left.is_zero() {
                    return true;
                }
                let (guard, _) = self
                    .wake
                    .wait_timeout(state, left)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state = guard;
            } else {
                state = self
                    .wake
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }
    }

    /// Park one delay after a failed sync so a persistent error does not spin.
    /// `false` when the group is stopping.
    ///
    /// The pause is [`DELAY`], not the configured backstop. A zero backstop
    /// would otherwise retry without sleeping.
    pub(super) fn wait_retry(&self) -> bool {
        let state = self.lock();
        if state.stop {
            return false;
        }
        let (guard, _) = self
            .wake
            .wait_timeout(state, DELAY)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        !guard.stop
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Stops and joins the delay thread when the last store clone drops.
///
/// The thread holds this value's [`GroupCommit`], not the database. Drop joins
/// the thread before the database handle is released.
pub(super) struct EngineLifetime {
    group: Arc<GroupCommit>,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl EngineLifetime {
    pub(super) fn new(group: Arc<GroupCommit>) -> Arc<Self> {
        Arc::new(Self {
            group,
            join: Mutex::new(None),
        })
    }

    pub(super) fn spawn(
        &self,
        group: Arc<GroupCommit>,
        mut sync: impl FnMut() -> eyre::Result<()> + Send + 'static,
    ) -> eyre::Result<()> {
        let join = std::thread::Builder::new()
            .name("irys-idx-group".into())
            .spawn(move || run_delay(&group, &mut sync))
            .wrap_err("spawn submodule index group thread")?;
        let mut slot = self
            .join
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        slot.replace(join);
        Ok(())
    }
}

impl Drop for EngineLifetime {
    fn drop(&mut self) {
        self.group.request_stop();
        let join = self
            .join
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(join) = join
            && join.join().is_err()
        {
            tracing::error!("submodule index group thread panicked");
        }
    }
}

fn run_delay(group: &GroupCommit, sync: &mut impl FnMut() -> eyre::Result<()>) {
    loop {
        // Stop does one last sync so the final registrations are durable
        // before the database handle closes.
        if !group.wait_until_due() {
            if let Err(err) = sync() {
                tracing::error!(%err, "submodule index group sync failed");
            }
            return;
        }
        if let Err(err) = sync() {
            tracing::error!(%err, "submodule index group sync failed");
            if !group.wait_retry() {
                let _ = sync();
                return;
            }
        }
    }
}
