use std::sync::{Arc, OnceLock};

use reth_db::DatabaseEnv;

use crate::ChunkProvider;

#[derive(Debug, Clone)]
pub struct IrysRethProviderInner {
    pub db: Arc<DatabaseEnv>,
    pub chunk_provider: Arc<ChunkProvider>,
}

pub type IrysRethProvider = Arc<OnceLock<IrysRethProviderInner>>;
