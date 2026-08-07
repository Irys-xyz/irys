use crate::reth_db::DatabaseError;
use metrics::{Histogram, Label};
use reth_db::mdbx::TransactionKind;
use reth_db::mdbx::cursor::Cursor;
use reth_db::table::{Decode, Decompress, DupSort, Table, TableRow};
use reth_db::transaction::DbTx as _;
use reth_db::{Database, DatabaseEnv};
use reth_db_api::database_metrics::DatabaseMetrics;
use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard};
use std::time::Instant;
use tracing::{info, info_span};

use irys_utils::{
    DB_SCOPE_IRYS_CONSENSUS, DB_SCOPE_RETH_EVM, DB_TX_MUT_ACQUIRE_DURATION_SECONDS,
    DB_WRITER_GATE_WAIT_SECONDS, MDBX_RW_TX_SPAN,
};

/// Per-environment writer gates for `DatabaseEnv` MDBX environments.
///
/// MDBX allows one RW transaction per env; concurrent `begin_rw_txn` callers hit
/// `Error::Busy` and sleep 250 ms in reth-irys libmdbx. Waiting on the gate (an
/// OS mutex) parks waiters until the previous writer commits, so residual Busy
/// sleeps are rare when every production writer takes the gate before `tx_mut`.
///
/// Gates are keyed by the Rust wrapper address (`ptr::from_ref(env)`), not the
/// intrinsic MDBX env pointer. Independent wrappers therefore get independent
/// queues (consensus vs each submodule). Two wrappers over **one** MDBX env
/// would under-serialise and restore Busy sleeps — that happens once at
/// startup when `ensure_db_version_compatible` runs on a stack-local env
/// before it is moved into `Arc` (`init_irys_db`); that window is single-
/// threaded and ends before concurrent writers start. An address reused by a
/// later environment shares the earlier gate, which merely merges queues and
/// stays correct. Gate values are leaked (`&'static`) so guards borrow no
/// registry lock; the registry is append-only, bounded by environments opened
/// over the process lifetime.
static WRITER_GATES: LazyLock<Mutex<std::collections::HashMap<usize, &'static Mutex<()>>>> =
    LazyLock::new(Default::default);

fn writer_gate_for(env: &DatabaseEnv) -> &'static Mutex<()> {
    let key = std::ptr::from_ref(env) as usize;
    WRITER_GATES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .entry(key)
        .or_insert_with(|| Box::leak(Box::new(Mutex::new(()))))
}

/// RAII guard for [`lock_writer_gate`]. Drop to release the gate.
pub type WriterGateGuard = MutexGuard<'static, ()>;

/// Acquire `env`'s writer gate. Prefer [`IrysDatabaseExt::update_eyre`] /
/// [`IrysDatabaseExt::update_scoped`] for normal writes. Use this only when a
/// raw `tx_mut` must join the same serialization (long multi-statement writers,
/// tests that hold the lock across other work).
///
/// # Poisoning
///
/// Recovers from a poisoned mutex so one panicked writer does not permanently
/// wedge writes to this environment.
///
/// # Reentrancy
///
/// The gate is not reentrant. While a thread holds this guard, it must not call
/// [`IrysDatabaseExt::update_eyre`], [`IrysDatabaseExt::update_eyre_at`], or
/// [`IrysDatabaseExt::update_scoped`] on the same environment; those methods take
/// the same gate and the thread would deadlock.
pub fn lock_writer_gate(env: &DatabaseEnv, call_site: &'static str) -> WriterGateGuard {
    let start = Instant::now();
    let mutex = writer_gate_for(env);
    // try_lock first so a contended (or nested same-thread) wait is visible in
    // debug builds; a nested lock on this non-reentrant mutex still deadlocks,
    // but the log names the waiter `call_site` before the park.
    let guard = match mutex.try_lock() {
        Ok(g) => g,
        Err(std::sync::TryLockError::WouldBlock) => {
            #[cfg(debug_assertions)]
            tracing::debug!(
                call_site,
                "writer gate contended; parking (nested same-thread acquire deadlocks)"
            );
            mutex
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }
        Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
    };
    metrics::histogram!(
        DB_WRITER_GATE_WAIT_SECONDS,
        "call_site" => call_site,
    )
    .record(start.elapsed().as_secs_f64());
    guard
}

// Cache the per-scope histogram handles so the hot rw-tx path doesn't re-resolve
// the recorder + per-call Key allocation on every `update_eyre`. Handles bind to
// the global recorder at first access, so `install_metrics_recorder()` must have
// run by the time any update_eyre is called — which is enforced by the call in
// chain/src/main.rs that runs before the DB is opened. Thread-local recorders
// (e.g. `metrics::with_local_recorder` in tests) are not visible to these
// statics; tests needing per-thread recorder isolation must not exercise this
// path.
static IRYS_CONSENSUS_TX_MUT_ACQUIRE_HISTOGRAM: LazyLock<Histogram> = LazyLock::new(|| {
    metrics::histogram!(
        DB_TX_MUT_ACQUIRE_DURATION_SECONDS,
        "scope" => DB_SCOPE_IRYS_CONSENSUS,
    )
});
static RETH_EVM_TX_MUT_ACQUIRE_HISTOGRAM: LazyLock<Histogram> = LazyLock::new(|| {
    metrics::histogram!(
        DB_TX_MUT_ACQUIRE_DURATION_SECONDS,
        "scope" => DB_SCOPE_RETH_EVM,
    )
});

/// In the reth library, there's a nested circular Arc reference. This circular dependency prevents
/// the DB connection from being dropped even when external references are removed, thereby making
/// it impossible to reopen the connection once all services has been stopped. As a workaround, this
/// DB wrapper forcibly disconnects the underlying database by taking the DB value out of its Option
/// once all associated services have been terminated. This is not the best solution to the problem,
/// but it was adopted after extensive analysis without a viable alternative.
///
/// If you wish to work on this a little bit more and solve this problem once and for all:
/// - One option to find where the circular dependency is and break it is to implement
///   `Drop` and `Clone` for the `RethDbWrapper` manually and see where it is cloned and dropped.
///   This will give you a better understanding of where it is used, and then you can do the same
///   for the structures that own `RethDbWrapper` to see whether or not they have any circular
///   links to each other.
#[derive(Clone, Debug)]
pub struct RethDbWrapper {
    db: Arc<RwLock<Option<DatabaseEnv>>>,
}

impl RethDbWrapper {
    #[must_use]
    pub fn new(db: DatabaseEnv) -> Self {
        Self {
            db: Arc::new(RwLock::new(Some(db))),
        }
    }

    /// Close underlying DB connection
    pub fn close(&self) {
        info!("Closing underlying DB connection");
        if let Ok(mut db) = self.db.write() {
            db.take();
        }
        info!("Connection Closed");
    }
}

fn db_read_error(_e: PoisonError<RwLockReadGuard<'_, Option<DatabaseEnv>>>) -> DatabaseError {
    DatabaseError::Other("Failed to acquire read lock on DB".to_string())
}

fn db_connection_closed_error() -> DatabaseError {
    DatabaseError::Other("DB connection has been closed".to_string())
}

impl reth_db::Database for RethDbWrapper {
    type TX = <DatabaseEnv as reth_db::Database>::TX;
    type TXMut = <DatabaseEnv as reth_db::Database>::TXMut;

    fn tx(&self) -> Result<Self::TX, DatabaseError> {
        let guard = self.db.read().map_err(db_read_error)?;
        guard.as_ref().ok_or_else(db_connection_closed_error)?.tx()
    }

    fn tx_mut(&self) -> Result<Self::TXMut, DatabaseError> {
        // Active span carries the EVM scope so any libmdbx writer-lock stall
        // warning fired during begin_rw_txn lands under
        // libmdbx_rw_tx_lock_stalls_total{scope="reth-evm"}.
        let _span = info_span!(MDBX_RW_TX_SPAN, db_scope = DB_SCOPE_RETH_EVM).entered();
        let guard = self.db.read().map_err(db_read_error)?;
        guard
            .as_ref()
            .ok_or_else(db_connection_closed_error)?
            .tx_mut()
    }

    fn view<T, F>(&self, f: F) -> Result<T, DatabaseError>
    where
        F: FnOnce(&mut Self::TX) -> T,
    {
        let guard = self.db.read().map_err(db_read_error)?;
        guard
            .as_ref()
            .ok_or_else(db_connection_closed_error)?
            .view(f)
    }

    fn update<T, F>(&self, f: F) -> Result<T, DatabaseError>
    where
        F: FnOnce(&Self::TXMut) -> T,
    {
        // See tx_mut() above — same scope attribution for Database::update.
        let _span = info_span!(MDBX_RW_TX_SPAN, db_scope = DB_SCOPE_RETH_EVM).entered();
        let guard = self.db.read().map_err(db_read_error)?;
        guard
            .as_ref()
            .ok_or_else(db_connection_closed_error)?
            .update(f)
    }

    fn path(&self) -> PathBuf {
        let guard = self.db.read().expect("failed to acquire read lock on DB");
        guard.as_ref().map(Database::path).unwrap_or_default()
    }
}

pub trait IrysDatabaseExt: reth_db::Database {
    fn update_eyre<T, F>(&self, f: F) -> eyre::Result<T>
    where
        F: FnOnce(&Self::TXMut) -> eyre::Result<T>;

    /// Like [`Self::update_eyre`] but labels the per-env writer-gate wait with
    /// `call_site` (histogram `db.writer_gate_wait_seconds`) on [`DatabaseEnv`].
    ///
    /// On [`RethDbWrapper`] `call_site` is ignored: that impl takes no writer
    /// gate (separate MDBX environment) and records only reth-EVM scope spans
    /// and the reth `tx_mut` acquire histogram.
    fn update_eyre_at<T, F>(&self, call_site: &'static str, f: F) -> eyre::Result<T>
    where
        F: FnOnce(&Self::TXMut) -> eyre::Result<T>;

    /// Takes a function and passes a read-only transaction into it, making sure it's closed in the
    /// end of the execution. This functions allows for `eyre` results.
    fn view_eyre<T, F>(&self, f: F) -> eyre::Result<T>
    where
        F: FnOnce(&Self::TX) -> eyre::Result<T>;

    /// Drop-in replacement for [`reth_db::Database::update`] that attributes any
    /// libmdbx writer-lock stall warning fired during `begin_rw_txn` to the
    /// caller's database scope via a tracing span. Without this wrapper, the
    /// stall counter records `scope="unknown"` because Reth's `Database::update`
    /// impl on `DatabaseEnv` lives upstream and cannot be intercepted directly.
    /// Use this for every consensus-DB write that doesn't return an
    /// `eyre::Result` (those can use [`update_eyre`] instead).
    ///
    /// On [`DatabaseEnv`] this also takes the per-environment writer gate so
    /// concurrent writers park on an app mutex instead of the 250 ms Busy poll.
    /// On [`RethDbWrapper`] there is no gate (separate env).
    fn update_scoped<T, F>(&self, f: F) -> Result<T, DatabaseError>
    where
        F: FnOnce(&Self::TXMut) -> T;
}

impl IrysDatabaseExt for RethDbWrapper {
    fn update_eyre<T, F>(&self, f: F) -> eyre::Result<T>
    where
        F: FnOnce(&Self::TXMut) -> eyre::Result<T>,
    {
        self.update_eyre_at("reth_evm.update_eyre", f)
    }

    fn update_eyre_at<T, F>(&self, _call_site: &'static str, f: F) -> eyre::Result<T>
    where
        F: FnOnce(&Self::TXMut) -> eyre::Result<T>,
    {
        // Inline the body rather than delegating to DatabaseEnv::update_eyre so
        // libmdbx writer-lock stall warnings and the tx_mut acquire histogram
        // are attributed to scope="reth-evm" instead of the consensus scope.
        // No consensus writer gate: this is a separate MDBX environment.
        let _span = info_span!(MDBX_RW_TX_SPAN, db_scope = DB_SCOPE_RETH_EVM).entered();

        let guard = self.db.read().map_err(db_read_error)?;
        let db = guard.as_ref().ok_or_else(db_connection_closed_error)?;

        let start = Instant::now();
        let tx_result = db.tx_mut();
        RETH_EVM_TX_MUT_ACQUIRE_HISTOGRAM.record(start.elapsed().as_secs_f64());
        let tx = tx_result?;

        let res = f(&tx)?;
        tx.commit()?;
        Ok(res)
    }

    /// Takes a function and passes a read-only transaction into it, making sure it's closed in the
    /// end of the execution. This functions allows for `eyre` results.
    fn view_eyre<T, F>(&self, f: F) -> eyre::Result<T>
    where
        F: FnOnce(&Self::TX) -> eyre::Result<T>,
    {
        let guard = self.db.read().map_err(db_read_error)?;
        guard
            .as_ref()
            .ok_or_else(db_connection_closed_error)?
            .view_eyre(f)
    }

    fn update_scoped<T, F>(&self, f: F) -> Result<T, DatabaseError>
    where
        F: FnOnce(&Self::TXMut) -> T,
    {
        // RethDbWrapper's own Database::update impl already wraps the call in
        // an `mdbx_rw_tx` span carrying `db_scope=reth-evm`, so this trait
        // method just delegates — no second span or consensus gate.
        <Self as Database>::update(self, f)
    }
}

impl IrysDatabaseExt for DatabaseEnv {
    fn update_eyre<T, F>(&self, f: F) -> eyre::Result<T>
    where
        F: FnOnce(&Self::TXMut) -> eyre::Result<T>,
    {
        self.update_eyre_at("update_eyre", f)
    }

    fn update_eyre_at<T, F>(&self, call_site: &'static str, f: F) -> eyre::Result<T>
    where
        F: FnOnce(&Self::TXMut) -> eyre::Result<T>,
    {
        // Active span carries the consensus scope so any libmdbx writer-lock
        // stall warning fired during begin_rw_txn lands under
        // libmdbx_rw_tx_lock_stalls_total{scope="irys-consensus"}.
        let _span = info_span!(MDBX_RW_TX_SPAN, db_scope = DB_SCOPE_IRYS_CONSENSUS).entered();
        // Take this env's gate before begin_rw_txn so concurrent writers park
        // here instead of Busy-sleeping 250 ms in libmdbx.
        let _gate = lock_writer_gate(self, call_site);

        // Time tx_mut() acquisition (should be near-zero when all writers use the gate).
        let start = Instant::now();
        let tx_result = self.tx_mut();
        IRYS_CONSENSUS_TX_MUT_ACQUIRE_HISTOGRAM.record(start.elapsed().as_secs_f64());
        let tx = tx_result?;

        let res = f(&tx)?;
        tx.commit()?;
        Ok(res)
    }

    /// Takes a function and passes a read-only transaction into it, making sure it's closed in the
    /// end of the execution. This functions allows for `eyre` results.
    fn view_eyre<T, F>(&self, f: F) -> eyre::Result<T>
    where
        F: FnOnce(&Self::TX) -> eyre::Result<T>,
    {
        let tx = self.tx()?;

        let res = f(&tx)?;
        tx.commit()?;
        Ok(res)
    }

    fn update_scoped<T, F>(&self, f: F) -> Result<T, DatabaseError>
    where
        F: FnOnce(&Self::TXMut) -> T,
    {
        let _span = info_span!(MDBX_RW_TX_SPAN, db_scope = DB_SCOPE_IRYS_CONSENSUS).entered();
        let _gate = lock_writer_gate(self, "update_scoped");
        let start = Instant::now();
        let tx_result = self.tx_mut();
        IRYS_CONSENSUS_TX_MUT_ACQUIRE_HISTOGRAM.record(start.elapsed().as_secs_f64());
        let tx = tx_result?;
        let res = f(&tx);
        tx.commit()?;
        Ok(res)
    }
}

impl RethDbWrapper {
    fn with_inner<R: Default>(&self, op: &'static str, f: impl FnOnce(&DatabaseEnv) -> R) -> R {
        let guard = match self.db.read() {
            Ok(g) => g,
            Err(_) => {
                tracing::warn!(op, "RethDbWrapper metrics unavailable: read lock poisoned");
                return R::default();
            }
        };
        match guard.as_ref() {
            Some(db) => f(db),
            None => {
                tracing::debug!(op, "RethDbWrapper metrics unavailable: inner DB closed");
                R::default()
            }
        }
    }
}

impl DatabaseMetrics for RethDbWrapper {
    fn report_metrics(&self) {
        self.with_inner("report_metrics", DatabaseMetrics::report_metrics);
    }

    fn gauge_metrics(&self) -> Vec<(&'static str, f64, Vec<Label>)> {
        self.with_inner("gauge_metrics", DatabaseMetrics::gauge_metrics)
    }

    fn counter_metrics(&self) -> Vec<(&'static str, u64, Vec<Label>)> {
        self.with_inner("counter_metrics", DatabaseMetrics::counter_metrics)
    }

    fn histogram_metrics(&self) -> Vec<(&'static str, f64, Vec<Label>)> {
        self.with_inner("histogram_metrics", DatabaseMetrics::histogram_metrics)
    }
}

pub trait IrysDupCursorExt<T: DupSort> {
    /// Count the number of dupilicates.
    fn dup_count(&mut self, key: T::Key) -> Result<Option<u32>, DatabaseError>;
}

pub fn decoder<'a, T>((k, v): (Cow<'a, [u8]>, Cow<'a, [u8]>)) -> Result<TableRow<T>, DatabaseError>
where
    T: Table,
    T::Key: Decode,
    T::Value: Decompress,
{
    Ok((
        match k {
            Cow::Borrowed(k) => Decode::decode(k)?,
            Cow::Owned(k) => Decode::decode_owned(k)?,
        },
        match v {
            Cow::Borrowed(v) => Decompress::decompress(v)?,
            Cow::Owned(v) => Decompress::decompress_owned(v)?,
        },
    ))
}

use reth_db::cursor::DbCursorRO as _;

impl<K: TransactionKind, T: DupSort> IrysDupCursorExt<T> for Cursor<K, T> {
    fn dup_count(&mut self, key: <T>::Key) -> Result<Option<u32>, DatabaseError> {
        Ok(
            // we seek to the key & check the key exists
            // if we pass a nonexistent key to get_dup_count, it'll panic
            match self.seek_exact(key)? {
                Some(_v) => Some(
                    self.inner
                        .get_dup_count()
                        .map_err(|e| DatabaseError::Read(e.into()))?,
                ),
                None => None,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::IrysTables;
    use crate::{IrysDatabaseArgs as _, open_or_create_db};
    use reth_db::mdbx::DatabaseArguments;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::thread;
    use std::time::Duration;

    /// Two overlapping `update_eyre` writers must be mutually exclusive.
    ///
    /// Overlap is guaranteed: the first writer signals from *inside* its write
    /// closure and the second writer starts only after that signal, so the gate
    /// is provably contended. Exclusion is asserted structurally via an
    /// occupancy counter rather than wall-clock arithmetic.
    ///
    /// Exclusion alone does not attribute the serialization to the gate —
    /// libmdbx enforces it too, after the loser pays the 250 ms Busy sleep.
    /// `writer_gate_blocks_txn_start_so_libmdbx_never_sees_a_busy_writer`
    /// covers that half.
    #[test]
    fn writer_gate_serializes_overlapping_writers_without_busy_floor() {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db = open_or_create_db(
            tmp.path(),
            IrysTables::ALL,
            DatabaseArguments::irys_testing().unwrap(),
        )
        .unwrap();
        let db = Arc::new(db);
        let first_writer_in_tx = Arc::new(Barrier::new(2));
        let occupancy = Arc::new(AtomicUsize::new(0));
        let max_occupancy = Arc::new(AtomicUsize::new(0));
        let hold = Duration::from_millis(10);

        let enter = |occupancy: &AtomicUsize, max_occupancy: &AtomicUsize| {
            let now = occupancy.fetch_add(1, Ordering::SeqCst) + 1;
            max_occupancy.fetch_max(now, Ordering::SeqCst);
        };

        let mut joins = Vec::new();
        for i in 0..2 {
            let db = Arc::clone(&db);
            let first_writer_in_tx = Arc::clone(&first_writer_in_tx);
            let occupancy = Arc::clone(&occupancy);
            let max_occupancy = Arc::clone(&max_occupancy);
            joins.push(thread::spawn(move || {
                if i == 0 {
                    db.update_eyre_at("test.concurrent_writer", |_tx| {
                        enter(&occupancy, &max_occupancy);
                        first_writer_in_tx.wait();
                        thread::sleep(hold);
                        occupancy.fetch_sub(1, Ordering::SeqCst);
                        Ok(())
                    })
                    .unwrap();
                } else {
                    first_writer_in_tx.wait();
                    db.update_eyre_at("test.concurrent_writer", |_tx| {
                        enter(&occupancy, &max_occupancy);
                        occupancy.fetch_sub(1, Ordering::SeqCst);
                        Ok(())
                    })
                    .unwrap();
                }
            }));
        }

        for j in joins {
            j.join().expect("writer thread panicked");
        }

        // Occupancy alone proves serialization; a wall-clock bound on gate wait
        // is load-sensitive under CI and is not required.
        assert_eq!(
            max_occupancy.load(Ordering::SeqCst),
            1,
            "two writers were inside their write closures at once; gate failed to serialize"
        );
    }

    /// A writer must park on the gate *before* `begin_rw_txn`, so libmdbx never
    /// sees two concurrent writers and the 250 ms Busy retry is unreachable.
    ///
    /// Asserted as a negative under a held gate rather than as a duration
    /// bound: the contended writer must not have entered its closure while the
    /// gate is held. A slow or loaded machine only delays that thread further,
    /// so load cannot turn a passing run into a failing one — only an actual
    /// regression (a writer reaching libmdbx while another writer is open) can.
    #[test]
    fn writer_gate_blocks_txn_start_so_libmdbx_never_sees_a_busy_writer() {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db = open_or_create_db(
            tmp.path(),
            IrysTables::ALL,
            DatabaseArguments::irys_testing().unwrap(),
        )
        .unwrap();
        let db = Arc::new(db);
        let writer_spawned = Arc::new(Barrier::new(2));
        let entered_closure = Arc::new(AtomicBool::new(false));

        let gate = lock_writer_gate(&db, "test.hold_gate");

        let writer = thread::spawn({
            let db = Arc::clone(&db);
            let writer_spawned = Arc::clone(&writer_spawned);
            let entered_closure = Arc::clone(&entered_closure);
            move || {
                writer_spawned.wait();
                db.update_eyre_at("test.gated_writer", |_tx| {
                    entered_closure.store(true, Ordering::SeqCst);
                    Ok(())
                })
                .unwrap();
            }
        });

        // The writer is running and can only be inside `lock_writer_gate`.
        writer_spawned.wait();
        thread::sleep(Duration::from_millis(50));
        assert!(
            !entered_closure.load(Ordering::SeqCst),
            "writer opened a txn while the gate was held; it would reach libmdbx contended and pay the Busy sleep"
        );

        drop(gate);
        writer.join().expect("writer thread panicked");
        assert!(
            entered_closure.load(Ordering::SeqCst),
            "writer never ran after the gate was released"
        );
    }
}
