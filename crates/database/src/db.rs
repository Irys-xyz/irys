use crate::reth_db::DatabaseError;
use reth_db::DatabaseEnv;
use reth_db_api::database_metrics::{DatabaseMetadata, DatabaseMetadataValue, DatabaseMetrics};
use std::sync::Arc;
use std::sync::RwLock;

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
        if let Ok(mut db) = self.db.write() {
            db.take();
        }
    }
}

impl reth_db::Database for RethDbWrapper {
    type TX = <DatabaseEnv as reth_db::Database>::TX;
    type TXMut = <DatabaseEnv as reth_db::Database>::TXMut;

    fn tx(&self) -> Result<Self::TX, DatabaseError> {
        let guard = self.db.read().unwrap();
        guard.as_ref().unwrap().tx()
    }

    fn tx_mut(&self) -> Result<Self::TXMut, DatabaseError> {
        let guard = self.db.read().unwrap();
        guard.as_ref().unwrap().tx_mut()
    }

    fn view<T, F>(&self, f: F) -> Result<T, DatabaseError>
    where
        F: FnOnce(&Self::TX) -> T,
    {
        let guard = self.db.read().unwrap();
        guard.as_ref().unwrap().view(f)
    }

    fn view_eyre<T, F>(&self, f: F) -> eyre::Result<T>
    where
        F: FnOnce(&Self::TX) -> eyre::Result<T>,
    {
        let guard = self.db.read().unwrap();
        guard.as_ref().unwrap().view_eyre(f)
    }

    fn update<T, F>(&self, f: F) -> Result<T, DatabaseError>
    where
        F: FnOnce(&Self::TXMut) -> T,
    {
        let guard = self.db.read().unwrap();
        guard.as_ref().unwrap().update(f)
    }

    fn update_eyre<T, F>(&self, f: F) -> eyre::Result<T>
    where
        F: FnOnce(&Self::TXMut) -> eyre::Result<T>,
    {
        let guard = self.db.read().unwrap();
        guard.as_ref().unwrap().update_eyre(f)
    }
}

impl DatabaseMetrics for RethDbWrapper {}

impl DatabaseMetadata for RethDbWrapper {
    fn metadata(&self) -> DatabaseMetadataValue {
        let guard = self.db.read().unwrap();
        guard.as_ref().unwrap().metadata()
    }
}
