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

use reth_db::table::{DupSort, Table};
use reth_db::{
    Database, DatabaseEnv, DatabaseError,
    cursor::{DbCursorRO, DbCursorRW, DbDupCursorRO, DbDupCursorRW},
    transaction::{DbTx as _, DbTxMut as _},
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

/// A database scope (consensus or cache).
pub trait DbScope: 'static + Send + Sync {}
impl DbScope for Consensus {}
impl DbScope for Cache {}

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
