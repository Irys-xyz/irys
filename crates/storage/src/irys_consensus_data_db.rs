use irys_database::tables::IrysTables;
use irys_database::{IrysDatabaseArgs as _, open_or_create_db};
use irys_types::DbSyncMode;
use reth_db::DatabaseEnv;
use reth_db::mdbx::DatabaseArguments;
use std::path::Path;

pub fn open_or_create_irys_consensus_data_db(
    path: impl AsRef<Path>,
    sync_mode: DbSyncMode,
) -> eyre::Result<DatabaseEnv> {
    // Convenience for the sync-mode-only case (used by tests): build default args.
    open_or_create_irys_consensus_data_db_with_args(
        path,
        DatabaseArguments::irys_default(sync_mode)?,
    )
}

/// Like [`open_or_create_irys_consensus_data_db`] but the caller supplies fully-
/// built [`DatabaseArguments`] (sync mode, geometry, etc.) — typically via
/// [`irys_database::consensus_db_args`] — so DB tuning lives in one place instead
/// of being threaded through as individual parameters.
pub fn open_or_create_irys_consensus_data_db_with_args(
    path: impl AsRef<Path>,
    args: DatabaseArguments,
) -> eyre::Result<DatabaseEnv> {
    open_or_create_db(path, IrysTables::ALL, args)
}
