use irys_database::tables::{CacheTables, IrysTables};
use irys_database::{IrysDatabaseArgs as _, open_or_create_cache_db, open_or_create_db};
use irys_types::DbSyncMode;
use reth_db::DatabaseEnv;
use reth_db::mdbx::DatabaseArguments;
use std::path::Path;

pub fn open_or_create_irys_consensus_data_db(
    path: impl AsRef<Path>,
    sync_mode: DbSyncMode,
) -> eyre::Result<DatabaseEnv> {
    open_or_create_db(
        path,
        IrysTables::ALL,
        DatabaseArguments::irys_default(sync_mode)?,
    )
}

/// Open or create the cache-side MDBX environment.
///
/// Uses the wider growth/shrink geometry from `DatabaseArguments::irys_cache`
/// because the cache tables churn in large batches during pruning.
pub fn open_or_create_irys_cache_data_db(
    path: impl AsRef<Path>,
    sync_mode: DbSyncMode,
) -> eyre::Result<DatabaseEnv> {
    open_or_create_cache_db(path, CacheTables::ALL, sync_mode)
}
