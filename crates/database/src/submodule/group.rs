//! Opt-in group commit for one submodule index.
//!
//! The group stays off until a caller enables it. A registration is visible
//! when its write returns. The fsync waits for `txs_per_sync` registrations,
//! a later durable write, an explicit sync, or 50 ms after the first unsynced
//! write. The clock is [`Instant`]: this bound is in-process only.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use eyre::WrapErr as _;

/// How long a quiet index may leave a registration unsynced.
const DELAY: Duration = Duration::from_millis(50);

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
    /// First unsynced registration in the open group. The 50 ms bound starts
    /// here and does not move forward when more writes arrive.
    dirty_since: Option<Instant>,
}

impl GroupCommit {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            enabled: AtomicBool::new(false),
            syncs: AtomicU64::new(0),
            state: Mutex::new(State {
                stop: false,
                spawned: false,
                txs_per_sync: 1,
                generation: 0,
                synced_generation: 0,
                dirty_since: None,
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
    /// not sync again. MDBX commits are not synced that way.
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
                let left = DELAY.saturating_sub(since.elapsed());
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
