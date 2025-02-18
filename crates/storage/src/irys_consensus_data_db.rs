use std::path::PathBuf;
use reth_db::DatabaseEnv;
use irys_database::open_or_create_db;
use irys_database::tables::IrysTables;

pub fn open_or_create_irys_consensus_data_db(path: &PathBuf) -> eyre::Result<DatabaseEnv> {
    open_or_create_db(path, IrysTables::ALL, None)
}