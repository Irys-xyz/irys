use std::{marker::PhantomData, ops::Deref, sync::Arc};

use reth_db::{DatabaseEnv, DatabaseError};

use crate::scoped_tx::{Cache, Consensus, DbScope, ScopedTx, ScopedTxMut};

/// Scope-tagged MDBX environment wrapper.
///
/// `S` pins the env to a canonical scope (`Cache`, `Consensus`, `Submodule`,
/// `Reth`). Scope-aware tx factories hang off the wrapper, so callers write
/// `env.begin_ro()` / `env.begin_rw()` instead of naming the scope at every
/// call site. The wrapper derefs to the inner `Arc<DatabaseEnv>` so untyped
/// `reth_db::Database` operations (`tx()`, `update_eyre(...)`, etc.) continue
/// to work via auto-deref.
///
/// Multiple `Env<S>` instances of the same scope can coexist — submodules
/// hold their own `Env<Submodule>` per partition.
///
/// `Clone` / `Debug` are hand-rolled rather than derived because the scope
/// tag is a zero-sized marker that doesn't impl those traits — the derive
/// macros would add a spurious `S: Clone + Debug` bound.
pub struct Env<S: DbScope> {
    inner: Arc<DatabaseEnv>,
    _scope: PhantomData<fn() -> S>,
}

impl<S: DbScope> Clone for Env<S> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            _scope: PhantomData,
        }
    }
}

impl<S: DbScope> std::fmt::Debug for Env<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Env")
            .field("scope", &S::LABEL)
            .field("inner", &self.inner)
            .finish()
    }
}

impl<S: DbScope> Env<S> {
    pub fn new(env: Arc<DatabaseEnv>) -> Self {
        Self {
            inner: env,
            _scope: PhantomData,
        }
    }

    /// Escape hatch: the underlying shared env handle. Use when calling APIs
    /// that need `Arc<DatabaseEnv>` directly (e.g. cloning into another
    /// owner). Prefer the scoped tx factories for new code.
    pub fn arc(&self) -> &Arc<DatabaseEnv> {
        &self.inner
    }

    pub fn begin_ro(&self) -> Result<ScopedTx<S>, DatabaseError> {
        ScopedTx::<S>::begin_ro(&self.inner)
    }

    pub fn begin_rw(&self) -> Result<ScopedTxMut<S>, DatabaseError> {
        ScopedTxMut::<S>::begin_rw(&self.inner)
    }
}

impl<S: DbScope> Deref for Env<S> {
    type Target = Arc<DatabaseEnv>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

/// Dumb container that holds the consensus + cache MDBX envs.
///
/// Submodule envs are held elsewhere (per partition), each as its own
/// `Env<Submodule>`.
///
/// `Deref` targets the consensus env so legacy code that does
/// `db.tx()` / `db.update_eyre(...)` keeps operating on consensus without
/// modification. New code should prefer `db.consensus()` / `db.cache()` for
/// the typed wrappers.
#[derive(Debug, Clone)]
pub struct DatabaseProvider {
    consensus: Env<Consensus>,
    cache: Env<Cache>,
}

impl DatabaseProvider {
    pub fn new(consensus: Arc<DatabaseEnv>, cache: Arc<DatabaseEnv>) -> Self {
        Self {
            consensus: Env::new(consensus),
            cache: Env::new(cache),
        }
    }

    pub fn consensus(&self) -> &Env<Consensus> {
        &self.consensus
    }

    pub fn cache(&self) -> &Env<Cache> {
        &self.cache
    }
}

impl Deref for DatabaseProvider {
    type Target = Arc<DatabaseEnv>;

    fn deref(&self) -> &Self::Target {
        &self.consensus.inner
    }
}

/// Reth EVM env handle. Owned wrapper kept separate from `DatabaseProvider`
/// because the EVM lives in a distinct MDBX env governed by reth's own
/// connection-management invariants.
#[derive(Debug, Clone)]
pub struct RethDatabaseProvider(pub Arc<DatabaseEnv>);

impl Deref for RethDatabaseProvider {
    type Target = Arc<DatabaseEnv>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
