use crate::block_provider::BlockProvider;
use crate::BlockHash;
use reth_db::DatabaseEnv;
use std::{ops::Deref, sync::Arc};

pub struct AppState {}

#[derive(Debug, Clone)]
pub struct DatabaseProvider(pub Arc<DatabaseEnv>);

impl Deref for DatabaseProvider {
    type Target = Arc<DatabaseEnv>;

    fn deref(&self) -> &Self::Target {
        &self.0
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
