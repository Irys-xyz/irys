use reth_db::DatabaseEnv;
use std::{ops::Deref, sync::Arc};

pub struct AppState {}

/// Holds both MDBX environments.
///
/// `Deref` targets the consensus env so existing code that does
/// `db.tx()` / `db.update(...)` / `db.view(...)` continues to operate on
/// the consensus database without modification. Cache access is via the
/// dedicated `cache()` accessor and the `view_cache` / `update_cache`
/// extension methods provided by `irys_database`.
#[derive(Debug, Clone)]
pub struct DatabaseProvider {
    consensus: Arc<DatabaseEnv>,
    cache: Arc<DatabaseEnv>,
}

impl DatabaseProvider {
    pub fn new(consensus: Arc<DatabaseEnv>, cache: Arc<DatabaseEnv>) -> Self {
        Self { consensus, cache }
    }

    pub fn consensus(&self) -> &Arc<DatabaseEnv> {
        &self.consensus
    }

    pub fn cache(&self) -> &Arc<DatabaseEnv> {
        &self.cache
    }
}

impl Deref for DatabaseProvider {
    type Target = Arc<DatabaseEnv>;

    fn deref(&self) -> &Self::Target {
        &self.consensus
    }
}

#[derive(Debug, Clone)]
pub struct RethDatabaseProvider(pub Arc<DatabaseEnv>);

impl Deref for RethDatabaseProvider {
    type Target = Arc<DatabaseEnv>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
