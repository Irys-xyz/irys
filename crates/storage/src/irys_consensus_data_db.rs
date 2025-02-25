use irys_database::open_or_create_db;
use irys_database::tables::IrysTables;
use reth_db::DatabaseEnv;
use std::path::PathBuf;

pub fn open_or_create_irys_consensus_data_db(path: &PathBuf) -> eyre::Result<DatabaseEnv> {
    open_or_create_db(path, IrysTables::ALL, None)
}
