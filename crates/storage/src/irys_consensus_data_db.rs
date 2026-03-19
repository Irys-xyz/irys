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
    open_or_create_db(
        path,
        IrysTables::ALL,
        DatabaseArguments::irys_default(sync_mode)?,
    )
}
