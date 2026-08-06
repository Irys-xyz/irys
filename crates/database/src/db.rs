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
    DB_CONSENSUS_WRITER_GATE_WAIT_SECONDS, DB_SCOPE_IRYS_CONSENSUS, DB_SCOPE_RETH_EVM,
    DB_TX_MUT_ACQUIRE_DURATION_SECONDS, MDBX_RW_TX_SPAN,
};

/// Process-wide gate for the irys-consensus MDBX environment's single writer.
///
/// MDBX allows one RW transaction per env; concurrent `begin_rw_txn` callers hit
/// `Error::Busy` and sleep 250 ms in reth-irys libmdbx. Waiting here (OS mutex)
/// parks waiters until the previous writer commits, so residual Busy sleeps are
/// rare when every production writer takes this gate before `tx_mut`.
///
/// Process-global (not per-env): all consensus `DatabaseEnv` instances in the
/// process share one queue. That is correct for a node (one consensus DB) and
/// only serializes writers across independent test DBs, which is safe.
static CONSENSUS_WRITER_GATE: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// RAII guard for [`lock_consensus_writer_gate`]. Drop to release the gate.
pub type ConsensusWriterGuard = MutexGuard<'static, ()>;

/// Acquire the consensus writer gate. Prefer [`IrysDatabaseExt::update_eyre`] /
/// [`IrysDatabaseExt::update_scoped`] for normal writes. Use this only when a
/// raw `tx_mut` must join the same serialization (long multi-statement writers,
/// tests that hold the lock across other work).
///
/// # Poisoning
///
/// Recovers from a poisoned mutex so one panicked writer does not permanently
/// wedge consensus writes.
pub fn lock_consensus_writer_gate(call_site: &'static str) -> ConsensusWriterGuard {
    let start = Instant::now();
    let guard = CONSENSUS_WRITER_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    metrics::histogram!(
        DB_CONSENSUS_WRITER_GATE_WAIT_SECONDS,
        "call_site" => call_site,
        "scope" => DB_SCOPE_IRYS_CONSENSUS,
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

    /// Like [`Self::update_eyre`] but records the consensus writer-gate wait under
    /// `call_site` (histogram `db.consensus_writer_gate_wait_seconds`).
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
    /// On the consensus DB this also takes the process-wide writer gate so
    /// concurrent writers park on an app mutex instead of the 250 ms Busy poll.
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
        // Take the app gate before begin_rw_txn so concurrent consensus writers
        // park here instead of Busy-sleeping 250 ms in libmdbx.
        let _gate = lock_consensus_writer_gate(call_site);

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
        let _gate = lock_consensus_writer_gate("update_scoped");
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
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::{Duration, Instant};

    /// Two concurrent `update_eyre` writers must serialize on the app gate so
    /// neither pays the 250 ms libmdbx Busy sleep. Wall-clock for both holds
    /// (~10 ms each) plus queue wait should stay well under one Busy quantum.
    #[test]
    fn consensus_writer_gate_avoids_250ms_busy_floor() {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db = open_or_create_db(
            tmp.path(),
            IrysTables::ALL,
            DatabaseArguments::irys_testing().unwrap(),
        )
        .unwrap();
        let db = Arc::new(db);
        let barrier = Arc::new(Barrier::new(2));
        let hold = Duration::from_millis(10);

        let mut joins = Vec::new();
        for i in 0..2 {
            let db = Arc::clone(&db);
            let barrier = Arc::clone(&barrier);
            joins.push(thread::spawn(move || {
                barrier.wait();
                let started = Instant::now();
                db.update_eyre_at("test.concurrent_writer", |_tx| {
                    thread::sleep(hold);
                    Ok(())
                })
                .unwrap();
                (i, started.elapsed())
            }));
        }

        let mut durations = Vec::new();
        for j in joins {
            durations.push(j.join().expect("writer thread panicked"));
        }

        // Both writers finish well under the Busy sleep floor (250 ms).
        // With gate: ~10 ms hold + ~10 ms wait ≈ 20–50 ms wall each in practice.
        // Without gate: loser would pay ≥250 ms on Busy.
        let max = durations.iter().map(|(_, d)| *d).max().unwrap();
        assert!(
            max < Duration::from_millis(150),
            "gated concurrent writers must finish without 250ms Busy floor; max={max:?} times={durations:?}"
        );
    }
}
