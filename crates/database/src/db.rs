use crate::reth_db::DatabaseError;
use reth_db::mdbx::cursor::Cursor;
use reth_db::mdbx::TransactionKind;
use reth_db::table::{Decode, Decompress, DupSort, Table, TableRow};
use reth_db::transaction::DbTx;
use reth_db::{Database as _, DatabaseEnv};
use reth_db_api::database_metrics::DatabaseMetrics;
use std::borrow::Cow;
use std::sync::RwLock;
use std::sync::{Arc, PoisonError, RwLockReadGuard};
use tracing::info;

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
        let guard = self.db.read().map_err(db_read_error)?;
        guard
            .as_ref()
            .ok_or_else(db_connection_closed_error)?
            .tx_mut()
    }

    fn view<T, F>(&self, f: F) -> Result<T, DatabaseError>
    where
        F: FnOnce(&Self::TX) -> T,
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
        let guard = self.db.read().map_err(db_read_error)?;
        guard
            .as_ref()
            .ok_or_else(db_connection_closed_error)?
            .update(f)
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
}

impl IrysDatabaseExt for RethDbWrapper {
    fn update_eyre<T, F>(&self, f: F) -> eyre::Result<T>
    where
        F: FnOnce(&Self::TXMut) -> eyre::Result<T>,
    {
        let guard = self.db.read().map_err(db_read_error)?;
        guard
            .as_ref()
            .ok_or_else(db_connection_closed_error)?
            .update_eyre(f)
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
}

impl IrysDatabaseExt for DatabaseEnv {
    fn update_eyre<T, F>(&self, f: F) -> eyre::Result<T>
    where
        F: FnOnce(&Self::TXMut) -> eyre::Result<T>,
    {
        let tx = self.tx_mut()?;

        let res = f(&tx);
        tx.commit()?;

        res
    }

    /// Takes a function and passes a read-only transaction into it, making sure it's closed in the
    /// end of the execution. This functions allows for `eyre` results.
    fn view_eyre<T, F>(&self, f: F) -> eyre::Result<T>
    where
        F: FnOnce(&Self::TX) -> eyre::Result<T>,
    {
        let tx = self.tx()?;

        let res = f(&tx);
        tx.commit()?;

        res
    }
}

impl DatabaseMetrics for RethDbWrapper {}

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

use reth_db::cursor::DbCursorRO;

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
