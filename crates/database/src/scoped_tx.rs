//! Type-safe scoped database transactions.
//!
//! The consensus and cache databases are separate MDBX environments. This
//! module defines marker types and wrapper transactions that prevent code
//! from accidentally touching a table from the wrong environment.
//!
//! Each table in `tables.rs` implements exactly one of [`ConsensusTable`] or
//! [`CacheTable`] (with the exception of metadata stamps, where each env has
//! its own table). A `ScopedTx<Consensus>` only exposes operations on
//! `ConsensusTable` impls; `ScopedTx<Cache>` only on `CacheTable` impls. A
//! cross-scope call is a compile error.

use std::marker::PhantomData;
use std::sync::LazyLock;

use metrics::Histogram;
use reth_db::table::{DupSort, Table, TableInfo};
use reth_db::{
    Database, DatabaseEnv, DatabaseError,
    cursor::{DbCursorRO, DbCursorRW, DbDupCursorRO, DbDupCursorRW},
    transaction::{DbTx as _, DbTxMut as _},
};
use tracing::info_span;

use irys_utils::{
    DB_SCOPE_IRYS_CACHE, DB_SCOPE_IRYS_CONSENSUS, DB_SCOPE_IRYS_SUBMODULE, DB_SCOPE_RETH_EVM,
    DB_TX_MUT_ACQUIRE_DURATION_SECONDS, MDBX_RW_TX_SPAN,
};

use crate::db::IrysDupCursorExt as _;

/// Marker trait: this table lives in the consensus MDBX env.
pub trait ConsensusTable: Table {}

/// Marker trait: this table lives in the cache MDBX env.
pub trait CacheTable: Table {}

/// Zero-sized scope tag for the consensus env.
pub enum Consensus {}

/// Zero-sized scope tag for the cache env.
pub enum Cache {}

/// Zero-sized scope tag for per-partition Irys submodule envs. Each storage
/// submodule owns its own MDBX env; they all share the same `Submodule` scope
/// tag since the stall counter's `scope` label is a fixed vocabulary, not a
/// per-env identifier. Has no associated table-set marker — submodule writes
/// use raw transactions through `Env<Submodule>::update_eyre`.
///
/// **Per-submodule granularity is a planned follow-up.** Right now every
/// submodule shares one bucket (`scope="irys-submodule"`) — once submodule
/// load is non-trivial we'll need per-partition labels for triage. Sketch:
///
/// 1. Add an optional `instance: Option<Arc<str>>` field to `Env<S>`
///    carrying a stable short identifier (e.g. `partition_hash[..8]`).
///    Bounds prometheus cardinality at partition count.
/// 2. Plumb it through `begin_scoped_rw` / `enter_rw_tx_span` as a second
///    span field `db_instance`. Extend `MdbxLockMetricsLayer` (in
///    `irys-utils/src/mdbx_metrics.rs`) to read it and emit a parallel
///    `submodule_id` label on `libmdbx_rw_tx_lock_stalls_total`.
/// 3. The acquire-latency histogram cached per-scope (the `LazyLock<Histogram>`
///    statics below) can't pre-resolve per-SM handles — dynamic cardinality.
///    Either cache per-instance via `OnceLock<Histogram>` on each `Env<S>`,
///    or fall back to inline `metrics::histogram!` calls (recorder
///    amortises lookups; modest overhead).
/// 4. `report_db_gauges::<Submodule>(env)` invoked per submodule in the
///    storage-modules tick, with the instance label threaded through. The
///    `IrysScope::ALL_TABLES` machinery already supports it; just needs
///    the helper to accept an optional instance label.
pub enum Submodule {}

/// Zero-sized scope tag for the Reth EVM env. Has no associated table-set
/// marker — Reth uses untyped transactions — but participates in `DbScope`
/// so MDBX stall/latency attribution flows through the same machinery.
pub enum Reth {}

/// A database scope (consensus, cache, or the Reth EVM env).
///
/// Each scope carries its canonical metrics label and a cached
/// `db.tx_mut_acquire_duration_seconds` histogram handle so that rw-tx
/// acquisition can be attributed without callers ever naming the literal
/// scope string.
pub trait DbScope: 'static + Send + Sync {
    const LABEL: &'static str;
    fn acquire_histogram() -> &'static Histogram;
}

// Per-scope handles bind to the global recorder at first access, so
// `install_metrics_recorder()` must have run before any DB tx is opened —
// enforced by chain/src/main.rs running the install before opening the DB.
// Thread-local recorders (e.g. `metrics::with_local_recorder` in tests) are
// not visible to these statics; tests needing per-thread recorder isolation
// must not exercise this path.
static CONSENSUS_TX_MUT_ACQUIRE_HISTOGRAM: LazyLock<Histogram> = LazyLock::new(|| {
    metrics::histogram!(
        DB_TX_MUT_ACQUIRE_DURATION_SECONDS,
        "scope" => DB_SCOPE_IRYS_CONSENSUS,
    )
});
static CACHE_TX_MUT_ACQUIRE_HISTOGRAM: LazyLock<Histogram> = LazyLock::new(|| {
    metrics::histogram!(
        DB_TX_MUT_ACQUIRE_DURATION_SECONDS,
        "scope" => DB_SCOPE_IRYS_CACHE,
    )
});
static RETH_EVM_TX_MUT_ACQUIRE_HISTOGRAM: LazyLock<Histogram> = LazyLock::new(|| {
    metrics::histogram!(
        DB_TX_MUT_ACQUIRE_DURATION_SECONDS,
        "scope" => DB_SCOPE_RETH_EVM,
    )
});
static SUBMODULE_TX_MUT_ACQUIRE_HISTOGRAM: LazyLock<Histogram> = LazyLock::new(|| {
    metrics::histogram!(
        DB_TX_MUT_ACQUIRE_DURATION_SECONDS,
        "scope" => DB_SCOPE_IRYS_SUBMODULE,
    )
});

impl DbScope for Consensus {
    const LABEL: &'static str = DB_SCOPE_IRYS_CONSENSUS;
    fn acquire_histogram() -> &'static Histogram {
        &CONSENSUS_TX_MUT_ACQUIRE_HISTOGRAM
    }
}

impl DbScope for Cache {
    const LABEL: &'static str = DB_SCOPE_IRYS_CACHE;
    fn acquire_histogram() -> &'static Histogram {
        &CACHE_TX_MUT_ACQUIRE_HISTOGRAM
    }
}

impl DbScope for Submodule {
    const LABEL: &'static str = DB_SCOPE_IRYS_SUBMODULE;
    fn acquire_histogram() -> &'static Histogram {
        &SUBMODULE_TX_MUT_ACQUIRE_HISTOGRAM
    }
}

impl DbScope for Reth {
    const LABEL: &'static str = DB_SCOPE_RETH_EVM;
    fn acquire_histogram() -> &'static Histogram {
        &RETH_EVM_TX_MUT_ACQUIRE_HISTOGRAM
    }
}

/// Extension trait for Irys-owned scopes that have an associated table set.
///
/// `Reth` deliberately does *not* implement this: the Reth EVM env uses
/// `reth_db::Tables` upstream, and Irys-side table enumeration / gauge hooks
/// don't apply to it.
pub trait IrysScope: DbScope {
    type Tables: TableInfo + 'static;
    const ALL_TABLES: &'static [Self::Tables];
}

/// Enter the `mdbx_rw_tx` span attributed to scope `S`.
///
/// Use this when wrapping an external rw-tx entrypoint (e.g. delegating to
/// `reth_db::Database::update`) where we want libmdbx stall warnings counted
/// under the right scope, but the actual `tx_mut()` call is owned by the
/// inner helper so we can't time it ourselves. For paths we fully own,
/// prefer [`begin_scoped_rw`] — it pairs the span with the acquire histogram.
#[must_use = "the returned EnteredSpan must be bound to a local to stay live"]
pub fn enter_rw_tx_span<S: DbScope>() -> tracing::span::EnteredSpan {
    info_span!(MDBX_RW_TX_SPAN, db_scope = S::LABEL).entered()
}

/// Acquire a raw MDBX rw-tx with the scope's stall span entered and the
/// scope's acquire-latency histogram recorded.
///
/// libmdbx writer-lock stall warnings fire only inside `begin_rw_txn`, so the
/// span need only be live while `tx_mut()` is running. Both successful and
/// failed acquires are timed so genuinely-slow-then-failed waits are visible.
pub fn begin_scoped_rw<S: DbScope>(
    env: &DatabaseEnv,
) -> Result<<DatabaseEnv as Database>::TXMut, DatabaseError> {
    let _span = enter_rw_tx_span::<S>();
    let start = std::time::Instant::now();
    let tx_result = env.tx_mut();
    S::acquire_histogram().record(start.elapsed().as_secs_f64());
    tx_result
}

/// Read-only transaction scoped to a single DB environment.
pub struct ScopedTx<S: DbScope> {
    inner: <DatabaseEnv as Database>::TX,
    _scope: PhantomData<fn() -> S>,
}

/// Read-write transaction scoped to a single DB environment.
pub struct ScopedTxMut<S: DbScope> {
    inner: <DatabaseEnv as Database>::TXMut,
    _scope: PhantomData<fn() -> S>,
}

impl<S: DbScope> ScopedTx<S> {
    pub fn new(inner: <DatabaseEnv as Database>::TX) -> Self {
        Self {
            inner,
            _scope: PhantomData,
        }
    }

    /// Open a read-only transaction on `env` typed under scope `S`. No span /
    /// histogram instrumentation: MDBX read transactions don't take the writer
    /// lock and so never emit the stall warning that scope attribution is for.
    pub fn begin_ro(env: &DatabaseEnv) -> Result<Self, DatabaseError> {
        Ok(Self::new(env.tx()?))
    }

    /// Escape hatch: the underlying untyped reth transaction.
    ///
    /// Use only when crossing scope boundaries is unavoidable (e.g. the
    /// V3→V4 migration touches both consensus and cache tables in the same
    /// code path). Prefer the scoped methods otherwise.
    pub fn inner(&self) -> &<DatabaseEnv as Database>::TX {
        &self.inner
    }

    pub fn commit(self) -> Result<(), DatabaseError> {
        self.inner.commit()
    }
}

impl<S: DbScope> ScopedTxMut<S> {
    pub fn new(inner: <DatabaseEnv as Database>::TXMut) -> Self {
        Self {
            inner,
            _scope: PhantomData,
        }
    }

    /// Open a read-write transaction on `env` typed under scope `S`, with
    /// MDBX stall attribution and acquire-latency metrics driven by `S`.
    pub fn begin_rw(env: &DatabaseEnv) -> Result<Self, DatabaseError> {
        Ok(Self::new(begin_scoped_rw::<S>(env)?))
    }

    pub fn inner(&self) -> &<DatabaseEnv as Database>::TXMut {
        &self.inner
    }

    pub fn commit(self) -> Result<(), DatabaseError> {
        self.inner.commit()
    }
}

// ----- Consensus-scoped operations -----
impl ScopedTx<Consensus> {
    pub fn get<T: Table + ConsensusTable>(
        &self,
        key: T::Key,
    ) -> Result<Option<T::Value>, DatabaseError> {
        self.inner.get::<T>(key)
    }

    pub fn entries<T: Table + ConsensusTable>(&self) -> Result<usize, DatabaseError> {
        self.inner.entries::<T>()
    }

    pub fn cursor_read<T: Table + ConsensusTable>(
        &self,
    ) -> Result<impl DbCursorRO<T>, DatabaseError> {
        self.inner.cursor_read::<T>()
    }

    pub fn cursor_dup_read<T: DupSort + ConsensusTable>(
        &self,
    ) -> Result<impl DbDupCursorRO<T> + DbCursorRO<T>, DatabaseError> {
        self.inner.cursor_dup_read::<T>()
    }
}

impl ScopedTxMut<Consensus> {
    pub fn get<T: Table + ConsensusTable>(
        &self,
        key: T::Key,
    ) -> Result<Option<T::Value>, DatabaseError> {
        self.inner.get::<T>(key)
    }

    pub fn put<T: Table + ConsensusTable>(
        &self,
        key: T::Key,
        value: T::Value,
    ) -> Result<(), DatabaseError> {
        self.inner.put::<T>(key, value)
    }

    pub fn delete<T: Table + ConsensusTable>(
        &self,
        key: T::Key,
        value: Option<T::Value>,
    ) -> Result<bool, DatabaseError> {
        self.inner.delete::<T>(key, value)
    }

    pub fn entries<T: Table + ConsensusTable>(&self) -> Result<usize, DatabaseError> {
        self.inner.entries::<T>()
    }

    pub fn clear<T: Table + ConsensusTable>(&self) -> Result<(), DatabaseError> {
        self.inner.clear::<T>()
    }

    pub fn cursor_read<T: Table + ConsensusTable>(
        &self,
    ) -> Result<impl DbCursorRO<T>, DatabaseError> {
        self.inner.cursor_read::<T>()
    }

    pub fn cursor_write<T: Table + ConsensusTable>(
        &self,
    ) -> Result<impl DbCursorRW<T> + DbCursorRO<T>, DatabaseError> {
        self.inner.cursor_write::<T>()
    }

    pub fn cursor_dup_read<T: DupSort + ConsensusTable>(
        &self,
    ) -> Result<impl DbDupCursorRO<T> + DbCursorRO<T>, DatabaseError> {
        self.inner.cursor_dup_read::<T>()
    }

    pub fn cursor_dup_write<T: DupSort + ConsensusTable>(
        &self,
    ) -> Result<
        impl DbDupCursorRW<T> + DbDupCursorRO<T> + DbCursorRW<T> + DbCursorRO<T>,
        DatabaseError,
    > {
        self.inner.cursor_dup_write::<T>()
    }
}

// ----- Cache-scoped operations (same shape, CacheTable bound) -----
impl ScopedTx<Cache> {
    pub fn get<T: Table + CacheTable>(
        &self,
        key: T::Key,
    ) -> Result<Option<T::Value>, DatabaseError> {
        self.inner.get::<T>(key)
    }

    pub fn entries<T: Table + CacheTable>(&self) -> Result<usize, DatabaseError> {
        self.inner.entries::<T>()
    }

    pub fn cursor_read<T: Table + CacheTable>(&self) -> Result<impl DbCursorRO<T>, DatabaseError> {
        self.inner.cursor_read::<T>()
    }

    pub fn cursor_dup_read<T: DupSort + CacheTable>(
        &self,
    ) -> Result<impl DbDupCursorRO<T> + DbCursorRO<T>, DatabaseError> {
        self.inner.cursor_dup_read::<T>()
    }

    /// Count the number of duplicate entries for the given key in a DupSort table.
    pub fn dup_count<T: DupSort + CacheTable>(
        &self,
        key: T::Key,
    ) -> Result<Option<u32>, DatabaseError> {
        let mut cursor = self.inner.cursor_dup_read::<T>()?;
        cursor.dup_count(key)
    }
}

impl ScopedTxMut<Cache> {
    pub fn get<T: Table + CacheTable>(
        &self,
        key: T::Key,
    ) -> Result<Option<T::Value>, DatabaseError> {
        self.inner.get::<T>(key)
    }

    pub fn put<T: Table + CacheTable>(
        &self,
        key: T::Key,
        value: T::Value,
    ) -> Result<(), DatabaseError> {
        self.inner.put::<T>(key, value)
    }

    pub fn delete<T: Table + CacheTable>(
        &self,
        key: T::Key,
        value: Option<T::Value>,
    ) -> Result<bool, DatabaseError> {
        self.inner.delete::<T>(key, value)
    }

    pub fn entries<T: Table + CacheTable>(&self) -> Result<usize, DatabaseError> {
        self.inner.entries::<T>()
    }

    pub fn clear<T: Table + CacheTable>(&self) -> Result<(), DatabaseError> {
        self.inner.clear::<T>()
    }

    pub fn cursor_read<T: Table + CacheTable>(&self) -> Result<impl DbCursorRO<T>, DatabaseError> {
        self.inner.cursor_read::<T>()
    }

    pub fn cursor_write<T: Table + CacheTable>(
        &self,
    ) -> Result<impl DbCursorRW<T> + DbCursorRO<T>, DatabaseError> {
        self.inner.cursor_write::<T>()
    }

    pub fn cursor_dup_read<T: DupSort + CacheTable>(
        &self,
    ) -> Result<impl DbDupCursorRO<T> + DbCursorRO<T>, DatabaseError> {
        self.inner.cursor_dup_read::<T>()
    }

    pub fn cursor_dup_write<T: DupSort + CacheTable>(
        &self,
    ) -> Result<
        impl DbDupCursorRW<T> + DbDupCursorRO<T> + DbCursorRW<T> + DbCursorRO<T>,
        DatabaseError,
    > {
        self.inner.cursor_dup_write::<T>()
    }
}
