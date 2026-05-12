use crate::reth_db::DatabaseError;
use crate::scoped_tx::{Cache, Consensus, ScopedTx, ScopedTxMut};
use metrics::{Histogram, Label};
use reth_db::mdbx::TransactionKind;
use reth_db::mdbx::cursor::Cursor;
use reth_db::table::{Decode, Decompress, DupSort, Table, TableRow};
use reth_db::transaction::DbTx as _;
use reth_db::{Database, DatabaseEnv};
use reth_db_api::database_metrics::DatabaseMetrics;
use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, PoisonError, RwLock, RwLockReadGuard};
use tracing::{info, info_span};

use irys_utils::{DB_SCOPE_RETH_EVM, DB_TX_MUT_ACQUIRE_DURATION_SECONDS, MDBX_RW_TX_SPAN};

// Reth's EVM rw-tx isn't routed through our typed `ScopedTxMut`, so its
// scope-attribution histogram lives here. Cache + consensus handles are
// owned by `crate::scoped_tx::DbScope::acquire_histogram` instead.
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
    fn update_scoped<T, F>(&self, f: F) -> Result<T, DatabaseError>
    where
        F: FnOnce(&Self::TXMut) -> T;
}

impl IrysDatabaseExt for RethDbWrapper {
    fn update_eyre<T, F>(&self, f: F) -> eyre::Result<T>
    where
        F: FnOnce(&Self::TXMut) -> eyre::Result<T>,
    {
        // Inline the body rather than delegating to DatabaseEnv::update_eyre so
        // libmdbx writer-lock stall warnings and the tx_mut acquire histogram
        // are attributed to scope="reth-evm" instead of the consensus scope
        // the inner helper hardcodes.
        let _span = info_span!(MDBX_RW_TX_SPAN, db_scope = DB_SCOPE_RETH_EVM).entered();

        let guard = self.db.read().map_err(db_read_error)?;
        let db = guard.as_ref().ok_or_else(db_connection_closed_error)?;

        let start = std::time::Instant::now();
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
        // an `mdbx_rw_tx` span carrying `db_scope=reth-evm` (see line 104), so
        // this trait method just delegates — no second span needed.
        <Self as Database>::update(self, f)
    }
}

impl IrysDatabaseExt for DatabaseEnv {
    fn update_eyre<T, F>(&self, f: F) -> eyre::Result<T>
    where
        F: FnOnce(&Self::TXMut) -> eyre::Result<T>,
    {
        // `begin_scoped_rw::<Consensus>` enters the `mdbx_rw_tx` span carrying
        // `db_scope="irys-consensus"` and records the acquire-latency histogram
        // — both driven by the `Consensus` scope tag rather than literals.
        let tx = crate::scoped_tx::begin_scoped_rw::<Consensus>(self)?;
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
        let tx = crate::scoped_tx::begin_scoped_rw::<Consensus>(self)?;
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

pub trait DatabaseProviderCacheExt {
    fn update_cache_eyre<T, F>(&self, f: F) -> eyre::Result<T>
    where
        F: FnOnce(&ScopedTxMut<Cache>) -> eyre::Result<T>;

    fn view_cache_eyre<T, F>(&self, f: F) -> eyre::Result<T>
    where
        F: FnOnce(&ScopedTx<Cache>) -> eyre::Result<T>;

    fn update_cache<T, F>(&self, f: F) -> Result<T, DatabaseError>
    where
        F: FnOnce(&ScopedTxMut<Cache>) -> T;

    fn view_cache<T, F>(&self, f: F) -> Result<T, DatabaseError>
    where
        F: FnOnce(&ScopedTx<Cache>) -> T;

    /// Open a scoped cache rw-transaction with MDBX stall metrics attributed
    /// via the `Cache` scope tag. Use this when the rw-tx must outlive a single
    /// closure (e.g. iterating with a cursor and committing after the loop).
    fn cache_tx_mut_scoped(&self) -> Result<ScopedTxMut<Cache>, DatabaseError>;
}

impl DatabaseProviderCacheExt for irys_types::DatabaseProvider {
    fn update_cache_eyre<T, F>(&self, f: F) -> eyre::Result<T>
    where
        F: FnOnce(&ScopedTxMut<Cache>) -> eyre::Result<T>,
    {
        let tx = ScopedTxMut::<Cache>::begin_rw(self.cache())?;
        let res = f(&tx)?;
        tx.commit()?;
        Ok(res)
    }

    fn view_cache_eyre<T, F>(&self, f: F) -> eyre::Result<T>
    where
        F: FnOnce(&ScopedTx<Cache>) -> eyre::Result<T>,
    {
        let tx = ScopedTx::<Cache>::begin_ro(self.cache())?;
        let res = f(&tx)?;
        tx.commit()?;
        Ok(res)
    }

    fn update_cache<T, F>(&self, f: F) -> Result<T, DatabaseError>
    where
        F: FnOnce(&ScopedTxMut<Cache>) -> T,
    {
        let tx = ScopedTxMut::<Cache>::begin_rw(self.cache())?;
        let res = f(&tx);
        tx.commit()?;
        Ok(res)
    }

    fn view_cache<T, F>(&self, f: F) -> Result<T, DatabaseError>
    where
        F: FnOnce(&ScopedTx<Cache>) -> T,
    {
        let tx = ScopedTx::<Cache>::begin_ro(self.cache())?;
        let res = f(&tx);
        tx.commit()?;
        Ok(res)
    }

    fn cache_tx_mut_scoped(&self) -> Result<ScopedTxMut<Cache>, DatabaseError> {
        ScopedTxMut::<Cache>::begin_rw(self.cache())
    }
}

pub trait DatabaseProviderTestExt {
    fn for_testing(
        consensus_path: &std::path::Path,
        cache_path: &std::path::Path,
    ) -> eyre::Result<Self>
    where
        Self: Sized;
}

impl DatabaseProviderTestExt for irys_types::DatabaseProvider {
    fn for_testing(
        consensus_path: &std::path::Path,
        cache_path: &std::path::Path,
    ) -> eyre::Result<Self> {
        use crate::IrysDatabaseArgs as _;
        use crate::tables::{CacheTables, ConsensusTables};
        use reth_db::mdbx::DatabaseArguments;
        let consensus = crate::open_or_create_db(
            consensus_path,
            ConsensusTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;
        let cache = crate::open_or_create_db(
            cache_path,
            CacheTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;
        crate::migration::ensure_db_version_compatible(&consensus, &cache)?;
        Ok(Self::new(
            std::sync::Arc::new(consensus),
            std::sync::Arc::new(cache),
        ))
    }
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
